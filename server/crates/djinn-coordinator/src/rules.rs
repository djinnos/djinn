// djinn:allow-oversize — legacy coordinator rules module over size-guard threshold; split when touched substantively.
// Coordinator tick rules (ADR-034):
//   (1) Spike/research task closure → create planning task for Planner.
//   (2) Batch completion (all worker tasks closed under open epic) → new planning task.
//   (4) Epic throughput: tasks merged per hour per epic (rolling window, in-memory).
//
// All rules are deterministic — zero LLM calls.

use super::reentrance::{DispatchEvent, should_auto_dispatch_planner};
use super::*;
use djinn_core::clock::{Clock, SystemClock};
use djinn_core::models::IssueType;
use djinn_core::models::task::{PRIORITY_CRITICAL, PROPOSAL_REVIEW_TITLE_PREFIX};
use djinn_db::repositories::proposal::TerminalLinkedEvidenceSpikeOutcome;
use djinn_db::{EpicRepository, ProposalRepository};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Rolling window for throughput tracking.
pub(super) const THROUGHPUT_WINDOW: Duration = Duration::from_secs(60 * 60);

/// Title prefix marking a Planner `epic_breakdown` task as a proposal reconcile
/// pass for an amended building proposal. The full marker is exactly
/// `Reconcile proposal <short_id>: <title>` for reliable deduplication.
pub(super) const PROPOSAL_RECONCILE_TITLE_PREFIX: &str = "Reconcile proposal";

// ── Epic completion rules ─────────────────────────────────────────────────────

impl CoordinatorActor {
    /// Called when any task transitions to `closed`.
    ///
    /// Checks the two epic-level completion rules:
    /// 1. Spike/research closure → planning task for Planner.
    /// 2. All worker tasks closed under an open epic → planning task for Planner.
    ///
    /// Deduplicates by checking whether an open planning task already exists.
    pub(super) async fn on_task_closed(&mut self, task: &djinn_core::models::Task) {
        let Some(epic_id) = task.epic_id.as_deref() else {
            return;
        };

        let epic_repo = EpicRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let epic = match epic_repo.get(epic_id).await {
            Ok(Some(e)) => e,
            _ => return,
        };

        // Only fire rules when the epic itself is still open.
        if epic.status != "open" {
            return;
        }

        let task_repo = self.task_repo();

        // Rule 1: Spike or Research closure.
        // Only fire when all other worker tasks are also closed (epic is drained),
        // so the planner doesn't create new waves while work is still in progress.
        let is_spike_or_research = matches!(task.issue_type.as_str(), "spike" | "research");

        if is_spike_or_research {
            let all_tasks = match task_repo.list_by_epic(epic_id).await {
                Ok(t) => t,
                Err(_) => return,
            };
            let has_open_work = all_tasks.iter().any(|t| {
                t.id != task.id
                    && !matches!(
                        t.issue_type.as_str(),
                        "planning" | "decomposition" | "review"
                    )
                    && t.status != "closed"
            });
            if !has_open_work && !self.open_planning_task_exists(&task_repo, epic_id).await {
                if should_auto_dispatch_planner(
                    &self.db,
                    DispatchEvent::TaskClosed {
                        epic_id,
                        close_reason: task.close_reason.as_deref(),
                    },
                )
                .await
                {
                    self.create_planning_task_by_ids(
                        &task_repo,
                        epic_id,
                        &task.project_id,
                        "spike_research_complete",
                    )
                    .await;
                } else {
                    tracing::debug!(
                        epic_id,
                        trigger = "spike_research_complete",
                        "CoordinatorActor: auto-dispatch suppressed by reentrance guard"
                    );
                }
            }
            return; // Rule 1 fires; skip rule 2 for this event.
        }

        // Rule 2: Batch completion — all non-planning/review tasks closed.
        // (Planning tasks themselves don't trigger further planning.)
        let is_planning_or_review = matches!(
            task.issue_type.as_str(),
            "planning" | "decomposition" | "review"
        );
        if is_planning_or_review {
            return;
        }

        // Query all tasks under the epic.
        let all_tasks = match task_repo.list_by_epic(epic_id).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    epic_id,
                    error = %e,
                    "CoordinatorActor: failed to list epic tasks for batch completion check"
                );
                return;
            }
        };

        // Worker tasks = not planning / review.
        let worker_tasks: Vec<_> = all_tasks
            .iter()
            .filter(|t| {
                !matches!(
                    t.issue_type.as_str(),
                    "planning" | "decomposition" | "review"
                )
            })
            .collect();

        if worker_tasks.is_empty() {
            return;
        }

        // Batch completion: all worker tasks are closed AND no tasks are in_progress.
        let all_closed = worker_tasks.iter().all(|t| t.status == "closed");
        let any_in_progress = all_tasks.iter().any(|t| {
            matches!(
                t.status.as_str(),
                "in_progress"
                    | "in_task_review"
                    | "in_lead_intervention"
                    | "needs_task_review"
                    | "needs_lead_intervention"
            )
        });

        if all_closed
            && !any_in_progress
            && !self.open_planning_task_exists(&task_repo, epic_id).await
        {
            if should_auto_dispatch_planner(
                &self.db,
                DispatchEvent::TaskClosed {
                    epic_id,
                    close_reason: task.close_reason.as_deref(),
                },
            )
            .await
            {
                self.create_planning_task_by_ids(
                    &task_repo,
                    epic_id,
                    &task.project_id,
                    "batch_complete",
                )
                .await;
            } else {
                tracing::debug!(
                    epic_id,
                    trigger = "batch_complete",
                    "CoordinatorActor: auto-dispatch suppressed by reentrance guard"
                );
            }
        }
    }

    /// Persist evidence-spike terminal lifecycle when a closed task is still
    /// linked from a proposal as its current evidence spike.
    ///
    /// This is intentionally narrow: it only observes the live task event path,
    /// delegates classification/idempotency to the repository primitive, and
    /// resumes the in-process tribunal only after a valid receipt is recorded.
    pub(super) async fn persist_terminal_linked_spike_evidence_from_closed_task(
        &mut self,
        task: &djinn_core::models::Task,
    ) {
        if task.status != "closed" {
            tracing::debug!(
                task_id = %task.id,
                status = %task.status,
                outcome = "NotTerminal",
                "CoordinatorActor: linked evidence spike lifecycle skipped for non-terminal task"
            );
            return;
        }

        let repo = ProposalRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let proposal = match repo.find_by_linked_spike(&task.id).await {
            Ok(Some(proposal)) => proposal,
            Ok(None) => {
                tracing::debug!(
                    task_id = %task.id,
                    task_short_id = %task.short_id,
                    outcome = "NotLinked",
                    "CoordinatorActor: closed task is not a linked evidence spike"
                );
                return;
            }
            Err(e) => {
                tracing::warn!(
                    task_id = %task.id,
                    task_short_id = %task.short_id,
                    error = %e,
                    "CoordinatorActor: failed to look up proposal linked to evidence spike"
                );
                return;
            }
        };

        match repo
            .persist_terminal_linked_spike_evidence_lifecycle(
                &proposal.id,
                &task.id,
                &task.status,
                task.close_reason.as_deref(),
            )
            .await
        {
            Ok(TerminalLinkedEvidenceSpikeOutcome::EvidenceReceived) => {
                tracing::info!(
                    proposal_id = %proposal.id,
                    spike_task_id = %task.id,
                    outcome = "EvidenceReceived",
                    "CoordinatorActor: recorded linked evidence spike receipt"
                );
                self.resume_refinement_after_evidence_received(&proposal.id, &task.id)
                    .await;
            }
            Ok(TerminalLinkedEvidenceSpikeOutcome::EvidenceFailed { reason }) => tracing::info!(
                proposal_id = %proposal.id,
                spike_task_id = %task.id,
                reason = %reason,
                outcome = "EvidenceFailed",
                "CoordinatorActor: recorded linked evidence spike failure"
            ),
            Ok(TerminalLinkedEvidenceSpikeOutcome::AlreadyRecorded { event_kind }) => {
                tracing::debug!(
                    proposal_id = %proposal.id,
                    spike_task_id = %task.id,
                    event_kind = %event_kind,
                    outcome = "AlreadyRecorded",
                    "CoordinatorActor: linked evidence spike terminal lifecycle already recorded"
                );
            }
            Ok(TerminalLinkedEvidenceSpikeOutcome::NotLinked) => tracing::debug!(
                proposal_id = %proposal.id,
                spike_task_id = %task.id,
                outcome = "NotLinked",
                "CoordinatorActor: linked evidence spike lifecycle skipped; proposal no longer linked"
            ),
            Ok(TerminalLinkedEvidenceSpikeOutcome::NotTerminal) => tracing::debug!(
                proposal_id = %proposal.id,
                spike_task_id = %task.id,
                status = %task.status,
                outcome = "NotTerminal",
                "CoordinatorActor: linked evidence spike lifecycle skipped; task is not terminal"
            ),
            Err(e) => tracing::warn!(
                proposal_id = %proposal.id,
                spike_task_id = %task.id,
                error = %e,
                "CoordinatorActor: failed to persist linked evidence spike terminal lifecycle"
            ),
        }
    }

    /// `true` if an open proposal-reconcile `epic_breakdown` task already exists
    /// in the home project for this proposal (matched by exact reconcile title
    /// marker + proposal short_id).
    pub(super) async fn open_reconcile_task_exists(
        &self,
        task_repo: &djinn_db::TaskRepository,
        home_project_id: &str,
        proposal_short_id: &str,
    ) -> bool {
        let marker = format!("{PROPOSAL_RECONCILE_TITLE_PREFIX} {proposal_short_id}:");
        match task_repo.list_by_project(home_project_id).await {
            Ok(tasks) => tasks.iter().any(|t| {
                t.issue_type.as_str() == "epic_breakdown"
                    && t.status != "closed"
                    && t.title.starts_with(&marker)
            }),
            Err(_) => false,
        }
    }

    /// Dispatch a single proposal-reconcile Planner task for a `building`
    /// proposal whose latest revision has drifted beyond the last reconciled
    /// revision. Shared by the event trigger and the drift sweep.
    pub(super) async fn dispatch_proposal_reconcile(
        &self,
        repo: &ProposalRepository,
        proposal: &djinn_core::models::Proposal,
        trigger: &str,
    ) {
        // Home project = the proposal's first `primary` target. If targets are
        // absent, fall back to the project of the build's original breakdown task
        // so already-graduated targetless proposals can still be reconciled.
        let task_repo = self.task_repo();
        let fallback_project_id = match proposal.build_breakdown_task_id.as_deref() {
            Some(task_id) => match task_repo.resolve(task_id).await {
                Ok(Some(task)) => Some(task.project_id),
                Ok(None) => None,
                Err(e) => {
                    tracing::warn!(
                        proposal_id = %proposal.id,
                        task_id,
                        error = %e,
                        "CoordinatorActor: failed to resolve proposal breakdown task for reconcile fallback"
                    );
                    None
                }
            },
            None => None,
        };
        let home_project_id = match repo.targets(&proposal.id).await {
            Ok(targets) => targets
                .iter()
                .find(|t| t.role == "primary")
                .map(|t| t.project_id.clone())
                .or(fallback_project_id),
            Err(_) => fallback_project_id,
        };
        let Some(home_project_id) = home_project_id else {
            tracing::warn!(proposal_id = %proposal.id, "CoordinatorActor: no home project for proposal reconcile — skipping");
            return;
        };

        // Reconcile is only meaningful after the initial proposal breakdown has
        // graduated at least one epic. Before that, the still-open initial
        // breakdown task naturally re-reads the latest proposal revision when it
        // runs, so spawning a second planner task would race it and have no
        // graduated graph to reconcile.
        match repo.graduated_epics(&proposal.id).await {
            Ok(epics) if epics.is_empty() => {
                tracing::debug!(
                    proposal_id = %proposal.id,
                    proposal_short_id = %proposal.short_id,
                    trigger,
                    "CoordinatorActor: skipping proposal reconcile until initial breakdown graduates epics"
                );
                return;
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal.id,
                    error = %e,
                    "CoordinatorActor: failed to load graduated epics for proposal reconcile — skipping"
                );
                return;
            }
        }

        // Dedup: don't stack a second reconcile while one is still open. The
        // open task must single-flight/coalesce any later revisions.
        if self
            .open_reconcile_task_exists(&task_repo, &home_project_id, &proposal.short_id)
            .await
        {
            return;
        }

        let from_revision = proposal.last_reconciled_revision_seq.unwrap_or(0);
        let latest_revision = proposal.latest_revision_seq;
        let title = format!(
            "{PROPOSAL_RECONCILE_TITLE_PREFIX} {}: {}",
            proposal.short_id, proposal.title
        );
        let design = format!(
            "A building proposal was updated after graduation and now has unreconciled drift.\n\n\
             Proposal: `{}` ({}) — {}\n\
             Trigger: `{}`\n\
             Observed drift: last reconciled revision N = `{}`, latest revision = `{}`.\n\n\
             ## Operating manual\n\n\
             1. **Single-flight / coalesce first.** On open, re-read the latest proposal with \
                `proposal_show(id=\"{}\")`. If another revision landed after this task was created, \
                reconcile the latest diff in this same pass; do **not** spawn another reconcile task.\n\
             2. Read proposal revisions N=`last_reconciled_revision_seq` and N+1=`latest_revision_seq` \
                (and continue through the current latest head if coalesced) so you understand exactly \
                what changed since the build last reconciled.\n\
             3. Inspect the graduated build graph linked to this proposal: list the proposal's \
                graduated epics, call `epic_show` for each, and call `epic_tasks` for EVERY epic — \
                including closed epics and closed/merged tasks — before editing anything. Build a map of \
                what scope is already owned or already shipped across ALL graduated epics, not a subset. \
                Checking only some siblings is how duplicate work gets built: proposal 8now shipped the \
                same worker-submit / reviewer-approve CI-gate wiring twice because an `[amend]` epic was \
                created for scope an already-graduated sibling epic already owned.\n\
             4. For each graduated epic, decide one of these outcomes:\n\
                - **Unchanged / still required:** leave the epic and its tasks alone.\n\
                - **Open and not yet broken down:** patch the epic in place to match the amended \
                  proposal.\n\
                - **Partially built / in flight:** add clarifying comments or follow-up instructions to \
                  open tasks as needed, but do **not** re-scope running tasks. Finish-then-next-wave is \
                  the default.\n\
                - **Newly required work:** ONLY when no existing graduated epic or task — open, in \
                  flight, OR closed/merged — already covers the scope, create a new `[amend] ...` epic \
                  linked to this proposal, then let normal epic breakdown produce tasks. If an existing \
                  graduated epic/task already covers it, patch/extend that epic (or leave it) instead; \
                  never ship the same scope in two epics.\n\
               - **Obsolete work:** close or unlink only the obsolete epic subtree; do not disturb \
                 unrelated graduated epics. Use the scoped teardown tool \
                 `proposal_reconcile_obsolete_epic(proposal_id=..., epic_id=...)` for obsolete \
                 graduated epics after listing the proposal's graduated epics and their tasks. \
                 Do **not** use whole-build `proposal_stop_build` and do not hand-close or unlink \
                 unrelated work.\n\
             5. **Merged-work safety gate.** Before auto-closing any obsolete epic subtree, inspect every \
               task in that subtree. If any task has `merge_commit_sha IS NOT NULL`, the scoped \
               `proposal_reconcile_obsolete_epic` call returns a blocked response and records AI \
               proposal feedback. Treat that blocked response as terminal for this reconcile pass: \
               preserve all state, leave unrelated epics untouched, stop immediately, and do not mark \
               the proposal reconciled.\n\
             6. **In-flight rule.** Do NOT re-scope running tasks. Leave running work to finish and express \
                amendments as comments, next-wave work, or obsolete-subtree aborts only.\n\
            7. On success, after all required patches, new linked `[amend]` epics, and \
               `proposal_reconcile_obsolete_epic` teardown operations have succeeded against the \
               current latest proposal revision, call the reconcile completion surface for that \
               latest revision — `proposal_ac_set` / `proposal_repository::mark_reconciled` if you \
               are operating in code, or the MCP/control-plane proposal reconcile completion tool if \
               available. Mark reconciled only after you have reconciled the latest proposal head.\n\n\
             This task has no `epic_id` — that is expected (you operate one level above epics).",
            proposal.short_id,
            proposal.id,
            proposal.title,
            trigger,
            from_revision,
            latest_revision,
            proposal.id,
        );
        let ac = serde_json::json!([
            {"criterion": "Latest proposal revision re-read on task open and all drift from last_reconciled_revision_seq through the current latest revision reconciled in one pass without spawning another reconcile task", "met": false},
            {"criterion": "Graduated epics and tasks inspected before acting, with unchanged work left alone, not-yet-broken-down work patched in place, partially built work handled by comments/follow-up without re-scoping running tasks, newly required work represented by linked [amend] epics, and obsolete work retired only through proposal_reconcile_obsolete_epic for its own subtree", "met": false},
            {"criterion": "Every graduated epic and its tasks (including closed epics and closed/merged tasks) were enumerated across ALL siblings, and no new [amend] epic duplicates scope already covered by an existing graduated epic or task; overlapping scope is handled by patching/extending the existing epic or leaving it, never by creating a duplicate epic", "met": false},
            {"criterion": "Merged-work safety gate applied: any proposal_reconcile_obsolete_epic blocked response for merged work records AI proposal feedback, preserves all state, leaves unrelated epics untouched, stops the reconcile pass, and does not mark reconciled", "met": false},
            {"criterion": "On successful reconciliation, proposal marked reconciled for the latest revision via proposal_ac_set / mark_reconciled only after all required patches, new linked epics, and obsolete teardowns succeeded", "met": false}
        ])
        .to_string();

        match task_repo
            .create_in_project(
                &home_project_id,
                None,
                &title,
                &design,
                &design,
                IssueType::EpicBreakdown.as_str(),
                PRIORITY_CRITICAL,
                "planner",
                Some("open"),
                Some(&ac),
            )
            .await
        {
            Ok(task) => {
                // Attribute to the build owner so any new epics/commits resolve
                // to a real account (mirrors graduation and proposal review).
                if let Some(owner) = proposal.build_owner_user_id.as_deref() {
                    let _ = task_repo.set_created_by_user_id(&task.id, owner).await;
                }
                tracing::info!(
                    proposal_id = %proposal.id,
                    proposal_short_id = %proposal.short_id,
                    task_short_id = %task.short_id,
                    trigger,
                    "CoordinatorActor: dispatched proposal reconcile task"
                );
            }
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal.id,
                    error = %e,
                    "CoordinatorActor: failed to create proposal reconcile task"
                );
            }
        }
    }

    /// Called when an epic transitions to `closed`. If that epic was graduated
    /// from a `building` proposal, dispatch a Planner (Workflow E) to reconcile
    /// the proposal's acceptance criteria against what has landed and decide
    /// whether to complete it or spawn more epics.
    ///
    /// Fires on EVERY graduated-epic close (incremental), not only when the last
    /// one closes — so AC progress reflects work as it lands. Deduplicated by
    /// [`Self::open_proposal_review_task_exists`], so concurrent / re-emitted
    /// `epic.updated` events do not stack duplicate reviews.
    pub(super) async fn maybe_review_proposal_on_epic_close(
        &self,
        epic: &djinn_core::models::Epic,
    ) {
        let repo = ProposalRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let proposal = match repo.proposal_for_epic(&epic.id).await {
            Ok(Some(p)) => p,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(epic_id = %epic.id, error = %e, "CoordinatorActor: proposal_for_epic lookup failed");
                return;
            }
        };
        if proposal.status != "building" {
            return;
        }
        self.dispatch_proposal_review(&repo, &proposal, Some(&epic.project_id), "epic_close")
            .await;
    }

    /// Backfill sweep: dispatch a closeout review for any `building` proposal
    /// whose graduated epics have all closed but which has no open review task.
    /// Catches proposals drained before the review rule existed (or whose
    /// `epic.updated` was missed) so they don't sit in `building` forever.
    pub(super) async fn sweep_proposals_needing_review(&self) {
        let repo = ProposalRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let drained = match repo.drained_building_proposals().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "CoordinatorActor: drained_building_proposals query failed");
                return;
            }
        };
        for proposal in &drained {
            // No fallback project — a proposal with no primary target can't host
            // a review task; dispatch_proposal_review logs and skips it.
            self.dispatch_proposal_review(&repo, proposal, None, "backfill_sweep")
                .await;
        }
    }

    /// Backfill sweep: dispatch a reconcile task for any `building` proposal
    /// whose latest revision has not been reconciled into its graduated build.
    /// Catches missed `proposal.updated` events and relies on the normal
    /// reconcile-task dedup helper for single-flight behavior.
    pub(super) async fn sweep_proposals_needing_reconcile(&self) {
        let repo = ProposalRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let drifted = match repo.drift_building_proposals().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "CoordinatorActor: drift_building_proposals query failed");
                return;
            }
        };
        for proposal in &drifted {
            self.dispatch_proposal_reconcile(&repo, proposal, "backfill_sweep")
                .await;
        }
    }

    /// Called on `proposal.updated`: if a `building` proposal has unreconciled
    /// revision drift, dispatch one open reconcile task and let that task re-read
    /// the latest revision. Non-material/status-only updates are filtered by the
    /// drift fields on the proposal model.
    pub(super) async fn maybe_reconcile_proposal_on_update(
        &self,
        proposal: &djinn_core::models::Proposal,
    ) {
        if proposal.status != "building" {
            return;
        }
        let reconciled_seq = proposal.last_reconciled_revision_seq.unwrap_or(0);
        if !proposal.pending_reconcile && proposal.latest_revision_seq <= reconciled_seq {
            return;
        }
        let repo = ProposalRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        self.dispatch_proposal_reconcile(&repo, proposal, "proposal_updated")
            .await;
    }

    /// Dispatch a single proposal-review Planner task (Workflow E) for a
    /// `building` proposal, unless one is already open. Shared by the
    /// per-epic-close trigger and the backfill sweep.
    ///
    /// `fallback_project_id` is the home project used only when the proposal has
    /// no resolvable `primary` target (the event path passes the closing epic's
    /// project; the sweep passes `None`).
    pub(super) async fn dispatch_proposal_review(
        &self,
        repo: &ProposalRepository,
        proposal: &djinn_core::models::Proposal,
        fallback_project_id: Option<&str>,
        trigger: &str,
    ) {
        // Home project = the proposal's first `primary` target (where graduation
        // lands the breakdown task), falling back to the provided project.
        let home_project_id = match repo.targets(&proposal.id).await {
            Ok(targets) => targets
                .iter()
                .find(|t| t.role == "primary")
                .map(|t| t.project_id.clone())
                .or_else(|| fallback_project_id.map(str::to_string)),
            Err(_) => fallback_project_id.map(str::to_string),
        };
        let Some(home_project_id) = home_project_id else {
            tracing::warn!(proposal_id = %proposal.id, "CoordinatorActor: no home project for proposal review — skipping");
            return;
        };

        let task_repo = self.task_repo();

        // Dedup: don't stack a second review while one is still open.
        if self
            .open_proposal_review_task_exists(&task_repo, &home_project_id, &proposal.short_id)
            .await
        {
            return;
        }

        let title = format!(
            "{PROPOSAL_REVIEW_TITLE_PREFIX} {}: {}",
            proposal.short_id, proposal.title
        );
        let design = format!(
            "An epic graduated from proposal `{}` ({}) just closed.\n\n\
             Call `proposal_show(id=\"{}\")` for the spec + acceptance criteria, then review what the \
             closed epics delivered and reconcile each acceptance criterion:\n\
             - For every criterion the landed work now satisfies, mark it met via \
               `proposal_ac_set(id=\"{}\", acceptance_criteria=[…])` — send the full list in order, \
               each entry `{{\"met\": true|false}}`; cite the evidence in your summary. `proposal_ac_set` \
               is status-only: it does not edit the spec, bump the proposal revision, or clear \
               sign-offs.\n\
             - If a criterion is **invalid, unverifiable, misstated, or needs narrowing** during \
               closeout, repair the spec with `proposal_ac_amend(id=\"{}\", reason=\"…\", amendments=[…])`. The \
               call needs a concrete `reason` (what is wrong and why) for its audit trail, and is \
               a real spec edit that bumps the proposal revision, retains sign-offs, and writes a \
               board-visible audit trail. Use it for rewrite / drop / waive (waive keeps the \
               criterion visible with `waived: true`) — never to hide valid \
               but unmet work; if the work is real and unfinished, leave the criterion unmet (or \
               create a follow-on epic) instead of waiving it.\n\
             - If EVERY remaining criterion is now met (or has been validly amended/waived/dropped) \
               → call `proposal_complete(id=\"{}\", summary=\"…\")`.\n\
             - If gaps remain and all epics are closed → create the additional epic(s) with \
               `epic_create(..., proposal_id=\"{}\")`, then `submit_grooming(...)`.\n\
             - Otherwise (gaps remain but work is still in flight) just record the AC progress and \
               stop; you will be re-dispatched as further epics close.\n\n\
             This task has no `epic_id` — that is expected (you operate one level above epics).",
            proposal.short_id,
            proposal.id,
            proposal.id,
            proposal.id,
            proposal.id,
            proposal.id,
            proposal.id,
        );
        let ac = serde_json::json!([
            {"criterion": "Proposal spec read and the closed epics' delivery reconciled against each acceptance criterion (met flags updated via proposal_ac_set, with invalid/unverifiable/misstated/narrowed criteria repaired via proposal_ac_amend and a concrete audit reason)", "met": false},
            {"criterion": "Outcome recorded: proposal_complete when all remaining criteria are met or validly amended/waived/dropped, OR additional epics created for real gaps, OR progress saved with work still in flight — never completed with valid-but-unmet criteria still standing", "met": false},
        ])
        .to_string();

        match task_repo
            .create_in_project(
                &home_project_id,
                None,
                &title,
                &design,
                &design,
                IssueType::EpicBreakdown.as_str(),
                PRIORITY_CRITICAL,
                "planner",
                Some("open"),
                Some(&ac),
            )
            .await
        {
            Ok(task) => {
                // Attribute to the build owner so any new epics/commits resolve
                // to a real account (mirrors graduation).
                if let Some(owner) = proposal.build_owner_user_id.as_deref() {
                    let _ = task_repo.set_created_by_user_id(&task.id, owner).await;
                }
                tracing::info!(
                    proposal_id = %proposal.id,
                    proposal_short_id = %proposal.short_id,
                    task_short_id = %task.short_id,
                    trigger,
                    "CoordinatorActor: dispatched proposal review task"
                );
            }
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal.id,
                    error = %e,
                    "CoordinatorActor: failed to create proposal review task"
                );
            }
        }
    }

    /// `true` if an open proposal-review `epic_breakdown` task already exists in
    /// the home project for this proposal (matched by the review title marker +
    /// the proposal's short_id). Mirrors [`Self::open_planning_task_exists`].
    pub(super) async fn open_proposal_review_task_exists(
        &self,
        task_repo: &djinn_db::TaskRepository,
        home_project_id: &str,
        proposal_short_id: &str,
    ) -> bool {
        let marker = format!("{PROPOSAL_REVIEW_TITLE_PREFIX} {proposal_short_id}:");
        match task_repo.list_by_project(home_project_id).await {
            Ok(tasks) => tasks.iter().any(|t| {
                t.issue_type.as_str() == "epic_breakdown"
                    && t.status != "closed"
                    && t.title.starts_with(&marker)
            }),
            Err(_) => false,
        }
    }

    /// Returns `true` if there is already an open `planning` task under the epic.
    pub(super) async fn open_planning_task_exists(
        &self,
        task_repo: &djinn_db::TaskRepository,
        epic_id: &str,
    ) -> bool {
        match task_repo.list_by_epic(epic_id).await {
            Ok(tasks) => tasks.iter().any(|t| {
                matches!(t.issue_type.as_str(), "planning" | "decomposition")
                    && t.status != "closed"
            }),
            Err(_) => false,
        }
    }

    /// ADR-051 §7 — shared epic-eligibility check used by exit-recheck and
    /// the 15-min stale sweep.
    ///
    /// An epic is eligible for a new planning wave when:
    ///   - it still exists and is `open`,
    ///   - it has at least one non-planning worker task,
    ///   - all worker tasks are closed,
    ///   - no tasks are in a mid-flight status,
    ///   - no open planning/decomposition task exists.
    ///
    /// The active-planner guard and close_reason filter are applied by
    /// `should_auto_dispatch_planner` at the actual dispatch site; this
    /// helper only checks the board-shape preconditions so callers can
    /// avoid pointless queries.
    pub(super) async fn epic_is_eligible_for_next_wave(
        &self,
        task_repo: &djinn_db::TaskRepository,
        epic_id: &str,
    ) -> bool {
        let epic_repo = EpicRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let epic = match epic_repo.get(epic_id).await {
            Ok(Some(e)) => e,
            _ => return false,
        };
        if epic.status != "open" {
            return false;
        }

        let all_tasks = match task_repo.list_by_epic(epic_id).await {
            Ok(t) => t,
            Err(_) => return false,
        };

        let worker_tasks: Vec<_> = all_tasks
            .iter()
            .filter(|t| {
                !matches!(
                    t.issue_type.as_str(),
                    "planning" | "decomposition" | "review"
                )
            })
            .collect();

        if worker_tasks.is_empty() {
            return false;
        }
        if !worker_tasks.iter().all(|t| t.status == "closed") {
            return false;
        }
        let any_in_progress = all_tasks.iter().any(|t| {
            matches!(
                t.status.as_str(),
                "in_progress"
                    | "in_task_review"
                    | "in_lead_intervention"
                    | "needs_task_review"
                    | "needs_lead_intervention"
            )
        });
        if any_in_progress {
            return false;
        }
        if self.open_planning_task_exists(task_repo, epic_id).await {
            return false;
        }
        true
    }

    /// Create a planning task for the Planner under the given epic (by ID).
    /// Used by spike/research and batch-completion rules that already hold the IDs.
    pub(super) async fn create_planning_task_by_ids(
        &self,
        task_repo: &djinn_db::TaskRepository,
        epic_id: &str,
        project_id: &str,
        trigger: &str,
    ) {
        let title = format!("Plan next wave ({trigger})");
        match task_repo
            .create_in_project_with_provenance(
                project_id,
                Some(epic_id),
                EffectiveCreatorProvenance {
                    explicit_user_id: None,
                    source_task_id: None,
                    proposal_id: None,
                },
                &title,
                "Plan the next wave of work for this epic. Review completed work, update the roadmap, and create 3–5 tasks.",
                "",
                IssueType::Planning.as_str(),
                PRIORITY_CRITICAL,
                "system",
                Some("open"),
                None,
            )
            .await
        {
            Ok(t) => {
                tracing::info!(
                    epic_id,
                    task_short_id = %t.short_id,
                    trigger,
                    "CoordinatorActor: created planning task"
                );
            }
            Err(e) => {
                tracing::warn!(
                    epic_id,
                    trigger,
                    error = %e,
                    "CoordinatorActor: failed to create planning task"
                );
            }
        }
    }

    // ── Throughput tracking ───────────────────────────────────────────────────

    /// Record a task merge event for the given epic (updates in-memory rolling window).
    pub(super) fn record_merge_event(&mut self, epic_id: &str) {
        let events = self
            .throughput_events
            .entry(epic_id.to_owned())
            .or_default();
        events.push(SystemClock::new().now_instant());
        // Eagerly evict events outside the rolling window to bound memory.
        events.retain(|t| t.elapsed() < THROUGHPUT_WINDOW);
    }

    /// Evict expired throughput events to bound memory usage.
    pub(super) fn evict_throughput_events(&mut self) {
        for events in self.throughput_events.values_mut() {
            events.retain(|t| t.elapsed() < THROUGHPUT_WINDOW);
        }
        self.throughput_events.retain(|_, v| !v.is_empty());
    }

    /// Return a snapshot of tasks-merged-per-hour per epic (within the rolling window).
    pub fn throughput_snapshot(&self) -> HashMap<String, usize> {
        self.throughput_events
            .iter()
            .map(|(epic_id, events)| {
                let count = events
                    .iter()
                    .filter(|t| t.elapsed() < THROUGHPUT_WINDOW)
                    .count();
                (epic_id.clone(), count)
            })
            .collect()
    }

    /// Best-effort terminalization of the live `task_attempts` row when a task
    /// transitions to `closed` via a force-close path (ForceClose,
    /// UserOverride→Closed, or Planner reshape/superseded/duplicate).
    ///
    /// Maps the `close_reason` to `TaskAttemptOutcome::ForceClosed` for all
    /// force-close reasons (`force_closed`, `reshape`, `superseded`,
    /// `duplicate`). Natural completion (`close_reason = "completed"`) is
    /// terminalized by the PR poller path instead.
    ///
    /// Preserves replacement/decomposition context in `summary_json` when
    /// available (close reason, actor/source from the activity log, task
    /// short-id). The underlying repository helper is forward-only and
    /// idempotent, so duplicate event delivery and out-of-order handlers
    /// never create a duplicate row and never move a terminal attempt
    /// backward.
    ///
    /// Best-effort: lookup/write failures are logged and never propagate.
    pub(super) async fn terminalize_force_close_attempt(&self, task: &djinn_core::models::Task) {
        use djinn_core::models::task_attempt::TaskAttemptOutcome;

        let Some(close_reason) = task.close_reason.as_deref() else {
            return;
        };

        // Only terminalize for force-close reasons. Natural completion
        // ("completed") is handled by the PR poller terminalization path.
        if !is_force_close_reason(close_reason) {
            return;
        }

        // Look up the most recent transition activity entry for this task to
        // capture the actor and actor_role who performed the closure.
        let (actor_id, actor_role) = match TaskRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        )
        .list_activity(&task.id)
        .await
        {
            Ok(entries) => {
                let latest_transition = entries
                    .iter()
                    .rev()
                    .find(|e| e.event_type == "status_changed");
                match latest_transition {
                    Some(entry) => (Some(entry.actor_id.clone()), Some(entry.actor_role.clone())),
                    None => (None, None),
                }
            }
            Err(_) => (None, None),
        };

        let summary_json = serde_json::json!({
            "close_reason": close_reason,
            "actor_id": actor_id,
            "actor_role": actor_role,
            "task_short_id": task.short_id,
        })
        .to_string();

        let summary = format!(
            "Task force-closed ({close_reason}) by {actor}",
            actor = actor_id.as_deref().unwrap_or("unknown"),
        );

        crate::dispatch::attempt_lifecycle::advance_latest_to_terminal(
            &self.db,
            crate::dispatch::attempt_lifecycle::TerminalAdvancementParams {
                task_id: &task.id,
                role: "worker",
                outcome: TaskAttemptOutcome::ForceClosed,
                pr_url: task.pr_url.as_deref(),
                submit_ref: None,
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: Some(&summary),
                summary_json: Some(&summary_json),
                log_tail: None,
            },
        )
        .await;
    }
}

/// True when a `close_reason` represents a force-close path (not natural
/// completion). Covers ForceClose, UserOverride→Closed, and Planner
/// reshape/superseded/duplicate close reasons.
fn is_force_close_reason(reason: &str) -> bool {
    matches!(
        reason,
        djinn_core::models::task::CLOSE_REASON_FORCE_CLOSED
            | djinn_core::models::task::CLOSE_REASON_RESHAPE
            | djinn_core::models::task::CLOSE_REASON_SUPERSEDED
            | djinn_core::models::task::CLOSE_REASON_DUPLICATE
    )
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers;
    use djinn_core::models::{EvidenceFindings, NeedsEvidenceClaim};
    use djinn_db::repositories::proposal::evidence_lifecycle_kind;
    use djinn_db::{
        EpicRepository, ProposalCreateInput, ProposalDebateTrailCreateInput, ProposalRepository,
        TaskRepository,
    };
    use tokio::sync::broadcast;
    use tokio_util::sync::CancellationToken;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn spawn_coordinator(
        db: &Database,
        tx: &broadcast::Sender<DjinnEventEnvelope>,
    ) -> CoordinatorHandle {
        use crate::roles::RoleRegistry;
        use djinn_provider::catalog::health::HealthTracker;
        use djinn_slot::{ModelSlotConfig, SlotPoolConfig, SlotPoolHandle};

        let cancel = CancellationToken::new();
        let ctx = test_helpers::agent_context_from_db(db.clone(), cancel.clone());
        let pool = SlotPoolHandle::spawn(
            ctx,
            cancel.clone(),
            SlotPoolConfig {
                models: vec![ModelSlotConfig {
                    model_id: DEFAULT_MODEL_ID.to_owned(),
                    max_slots: 2,
                    roles: ["worker", "reviewer"]
                        .into_iter()
                        .map(ToOwned::to_owned)
                        .collect(),
                }],
                role_priorities: HashMap::new(),
            },
        );
        CoordinatorHandle::spawn(CoordinatorDeps::new(
            tx.clone(),
            cancel,
            db.clone(),
            pool,
            djinn_provider::catalog::CatalogService::new(),
            HealthTracker::new(),
            Arc::new(RoleRegistry::new()),
            BackgroundWorkTracker::default(),
            djinn_lsp::LspManager::new(),
        ))
    }

    /// Build a `CoordinatorActor` directly (not spawned) so a test can drive an
    /// individual method like `sweep_proposals_needing_review` deterministically.
    fn make_coordinator_actor(
        db: &Database,
        tx: &broadcast::Sender<DjinnEventEnvelope>,
    ) -> CoordinatorActor {
        use crate::roles::RoleRegistry;
        use djinn_provider::catalog::health::HealthTracker;
        use djinn_slot::{ModelSlotConfig, SlotPoolConfig, SlotPoolHandle};

        let cancel = CancellationToken::new();
        let ctx = test_helpers::agent_context_from_db(db.clone(), cancel.clone());
        let pool = SlotPoolHandle::spawn(
            ctx,
            cancel.clone(),
            SlotPoolConfig {
                models: vec![ModelSlotConfig {
                    model_id: DEFAULT_MODEL_ID.to_owned(),
                    max_slots: 1,
                    roles: ["worker"].into_iter().map(ToOwned::to_owned).collect(),
                }],
                role_priorities: HashMap::new(),
            },
        );
        let (status_tx, _) = tokio::sync::watch::channel(SharedCoordinatorState {
            dispatched: 0,
            recovered: 0,
            epic_throughput: HashMap::new(),
            pr_errors: HashMap::new(),
            rate_limited_until: None,
        });
        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        CoordinatorActor::new(
            CoordinatorDeps::new(
                tx.clone(),
                cancel,
                db.clone(),
                pool,
                djinn_provider::catalog::CatalogService::new(),
                HealthTracker::new(),
                Arc::new(RoleRegistry::new()),
                BackgroundWorkTracker::default(),
                djinn_lsp::LspManager::new(),
            ),
            receiver,
            sender,
            status_tx,
        )
    }

    async fn make_epic(
        db: &Database,
        project_id: &str,
        tx: &broadcast::Sender<DjinnEventEnvelope>,
    ) -> djinn_core::models::Epic {
        EpicRepository::new(db.clone(), crate::events::event_bus_for(tx))
            .create_for_project(
                project_id,
                djinn_db::EpicCreateInput {
                    title: "Test Epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("open"),
                    auto_breakdown: None,
                    originating_adr_id: None,
                    blocked_by: None,
                },
            )
            .await
            .unwrap()
    }

    async fn create_task(
        db: &Database,
        epic_id: &str,
        project_id: &str,
        title: &str,
        issue_type: &str,
        tx: &broadcast::Sender<DjinnEventEnvelope>,
    ) -> djinn_core::models::Task {
        TaskRepository::new(db.clone(), crate::events::event_bus_for(tx))
            .create_in_project(
                project_id,
                Some(epic_id),
                title,
                "",
                "",
                issue_type,
                0,
                "",
                Some("open"),
                None,
            )
            .await
            .unwrap()
    }

    async fn close_task(db: &Database, task_id: &str, tx: &broadcast::Sender<DjinnEventEnvelope>) {
        TaskRepository::new(db.clone(), crate::events::event_bus_for(tx))
            .transition(
                task_id,
                djinn_core::models::TransitionAction::Close,
                "test",
                "system",
                None,
                None,
            )
            .await
            .unwrap();
    }

    fn planning_count(tasks: &[djinn_core::models::Task]) -> usize {
        tasks
            .iter()
            .filter(|t| {
                matches!(t.issue_type.as_str(), "planning" | "decomposition")
                    && t.status != "closed"
            })
            .count()
    }

    fn sample_needs_evidence_claim() -> NeedsEvidenceClaim {
        NeedsEvidenceClaim {
            question: "Does the evidence spike support the refinement?".to_owned(),
            target_subsystem: "coordinator event path".to_owned(),
            spec_unknown_anchor: "linked spike terminal event".to_owned(),
            insufficient_in_session_research: "requires event-driven fixture".to_owned(),
            expected_findings: "structured evidence_findings".to_owned(),
            created_by_task_id: "judge-task-evt".to_owned(),
            round: 2,
            against_revision_seq: 1,
        }
    }

    fn sample_evidence_findings(answer: &str) -> EvidenceFindings {
        EvidenceFindings {
            answer: answer.to_owned(),
            evidence: vec!["coordinator fixture observed terminal task event".to_owned()],
            code_paths_inspected: vec!["server/crates/djinn-coordinator/src/rules.rs".to_owned()],
            confidence: 0.9,
            residual_risks: vec!["fixture-only evidence".to_owned()],
            recommendation_for_advocate: "incorporate the event evidence".to_owned(),
        }
    }

    async fn setup_linked_evidence_spike_fixture(
        db: &Database,
        tx: &broadcast::Sender<DjinnEventEnvelope>,
        title: &str,
    ) -> (
        ProposalRepository,
        TaskRepository,
        djinn_core::models::Proposal,
        djinn_core::models::Task,
        NeedsEvidenceClaim,
    ) {
        let event_bus = crate::events::event_bus_for(tx);
        let proposal_repo = ProposalRepository::new(db.clone(), event_bus.clone());
        let task_repo = TaskRepository::new(db.clone(), event_bus.clone());
        let project = test_helpers::create_test_project(db).await;
        let epic = make_epic(db, &project.id, tx).await;
        let proposal = proposal_repo
            .create(ProposalCreateInput {
                title,
                body: "body",
                acceptance_criteria: None,
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        let spike_task = create_task(db, &epic.id, &project.id, title, "spike", tx).await;
        let claim = sample_needs_evidence_claim();
        let proposal = proposal_repo
            .set_structured_needs_evidence_spike(&proposal.id, &spike_task.id, &claim)
            .await
            .unwrap();
        (proposal_repo, task_repo, proposal, spike_task, claim)
    }

    async fn count_evidence_lifecycle_events(
        repo: &ProposalRepository,
        proposal_id: &str,
        kind: &str,
    ) -> usize {
        repo.revisions(proposal_id)
            .await
            .unwrap()
            .iter()
            .filter(|rev| rev.event_kind == kind)
            .count()
    }

    async fn tribunal_task_count(task_repo: &TaskRepository, project_id: &str) -> usize {
        task_repo
            .list_by_project(project_id)
            .await
            .unwrap()
            .iter()
            .filter(|task| {
                matches!(
                    task.agent_type.as_deref(),
                    Some("advocate" | "adversary" | "judge")
                )
            })
            .count()
    }

    /// Count of open proposal-review (`epic_breakdown`) tasks for a proposal in
    /// the given project, matched the same way the coordinator dedups them.
    async fn review_task_count(
        task_repo: &TaskRepository,
        project_id: &str,
        proposal_short_id: &str,
    ) -> usize {
        let marker = format!("{PROPOSAL_REVIEW_TITLE_PREFIX} {proposal_short_id}:");
        task_repo
            .list_by_project(project_id)
            .await
            .unwrap()
            .iter()
            .filter(|t| {
                t.issue_type.as_str() == "epic_breakdown"
                    && t.status != "closed"
                    && t.title.starts_with(&marker)
            })
            .count()
    }

    /// Count of open proposal-reconcile (`epic_breakdown`) tasks for a proposal
    /// in the given project, matched by the exact reconcile marker and issue
    /// type (not a loose title contains).
    async fn reconcile_task_count(
        task_repo: &TaskRepository,
        project_id: &str,
        proposal_short_id: &str,
    ) -> usize {
        let marker = format!("{PROPOSAL_RECONCILE_TITLE_PREFIX} {proposal_short_id}:");
        task_repo
            .list_by_project(project_id)
            .await
            .unwrap()
            .iter()
            .filter(|t| {
                t.issue_type.as_str() == "epic_breakdown"
                    && t.status != "closed"
                    && t.title.starts_with(&marker)
            })
            .count()
    }

    async fn assert_reconcile_task_count(
        task_repo: &TaskRepository,
        project_id: &str,
        proposal_short_id: &str,
        expected: usize,
        message: &str,
    ) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let count = reconcile_task_count(task_repo, project_id, proposal_short_id).await;
            if count == expected {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                assert_eq!(count, expected, "{message}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    async fn assert_review_task_count(
        task_repo: &TaskRepository,
        project_id: &str,
        proposal_short_id: &str,
        expected: usize,
        message: &str,
    ) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let count = review_task_count(task_repo, project_id, proposal_short_id).await;
            if count == expected {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                assert_eq!(count, expected, "{message}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    async fn assert_planning_task_count(
        task_repo: &TaskRepository,
        epic_id: &str,
        expected: usize,
        message: &str,
    ) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let tasks = task_repo.list_by_epic(epic_id).await.unwrap();
            let count = planning_count(&tasks);
            if count == expected {
                return;
            }

            if tokio::time::Instant::now() >= deadline {
                assert_eq!(count, expected, "{message}");
            }

            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    // ── AC1: Spike/research closure → decomposition task ──────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spike_closure_creates_decomposition_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let epic = make_epic(&db, &project.id, &tx).await;

        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        let spike = create_task(&db, &epic.id, &project.id, "Spike task", "spike", &tx).await;

        let _handle = spawn_coordinator(&db, &tx);

        // Close the spike task — should trigger decomposition task creation.
        close_task(&db, &spike.id, &tx).await;
        assert_planning_task_count(
            &task_repo,
            &epic.id,
            1,
            "spike closure should create exactly one planning task",
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn research_closure_creates_decomposition_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let epic = make_epic(&db, &project.id, &tx).await;

        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        let research =
            create_task(&db, &epic.id, &project.id, "Research task", "research", &tx).await;

        let _handle = spawn_coordinator(&db, &tx);

        close_task(&db, &research.id, &tx).await;
        assert_planning_task_count(
            &task_repo,
            &epic.id,
            1,
            "research closure should create one planning task",
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spike_closure_does_not_duplicate_decomposition_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let epic = make_epic(&db, &project.id, &tx).await;
        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        // Pre-create an open planning task.
        create_task(&db, &epic.id, &project.id, "Existing plan", "planning", &tx).await;

        let spike = create_task(&db, &epic.id, &project.id, "Spike", "spike", &tx).await;

        let _handle = spawn_coordinator(&db, &tx);
        close_task(&db, &spike.id, &tx).await;
        // Negative assertion: count must *stay* at 1 (the pre-existing planning
        // task). `assert_planning_task_count(1)` wouldn't help — it returns as
        // soon as count matches, missing a late spurious write. 400ms gives
        // the coordinator tick + DB write window under concurrent nextest
        // load; 150ms was too tight.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        let tasks = task_repo.list_by_epic(&epic.id).await.unwrap();
        assert_eq!(
            planning_count(&tasks),
            1,
            "should not create a duplicate planning task when one already exists"
        );
    }

    // ── AC2: Batch completion → decomposition task ─────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn batch_completion_creates_decomposition_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let epic = make_epic(&db, &project.id, &tx).await;
        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        let t1 = create_task(&db, &epic.id, &project.id, "Task 1", "task", &tx).await;
        let t2 = create_task(&db, &epic.id, &project.id, "Task 2", "feature", &tx).await;

        let _handle = spawn_coordinator(&db, &tx);

        // Close t1 first — epic not yet complete.
        close_task(&db, &t1.id, &tx).await;
        // Give the coordinator a bounded window to process t1's close event.
        // A plain `sleep(100ms)` flaked under concurrent nextest load (the
        // tick wakes up but the event hasn't landed yet); the assertion
        // below is a *negative* one (count must stay 0) so we can't use
        // `assert_planning_task_count(0)` — that helper returns as soon as
        // the count matches and wouldn't guard against a late write. Keep
        // the sleep but lift the budget to 400ms, which empirically covers
        // the coordinator tick + DB write window under load.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        let tasks = task_repo.list_by_epic(&epic.id).await.unwrap();
        assert_eq!(
            planning_count(&tasks),
            0,
            "partial completion should not create planning task"
        );

        // Close t2 — batch complete now. Poll (up to the helper's 10s
        // deadline) rather than `sleep + assert_eq`: under parallel load
        // the coordinator can take several hundred ms to pick up the
        // close event and issue the planning-task write.
        close_task(&db, &t2.id, &tx).await;
        assert_planning_task_count(
            &task_repo,
            &epic.id,
            1,
            "batch completion should create exactly one planning task",
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn batch_completion_skipped_when_decomposition_exists() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let epic = make_epic(&db, &project.id, &tx).await;
        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        let t1 = create_task(&db, &epic.id, &project.id, "Task 1", "task", &tx).await;
        // Pre-existing open planning task.
        create_task(&db, &epic.id, &project.id, "Existing plan", "planning", &tx).await;

        let _handle = spawn_coordinator(&db, &tx);
        close_task(&db, &t1.id, &tx).await;
        // Negative assertion — same 400ms budget as the sibling test above.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        let tasks = task_repo.list_by_epic(&epic.id).await.unwrap();
        assert_eq!(
            planning_count(&tasks),
            1,
            "should not create duplicate planning task on batch completion"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn batch_completion_does_not_fire_for_closed_epic() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let epic = make_epic(&db, &project.id, &tx).await;
        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        let t1 = create_task(&db, &epic.id, &project.id, "Task 1", "task", &tx).await;

        // Close the epic first.
        EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx))
            .close(&epic.id)
            .await
            .unwrap();

        let closed_child = task_repo.get(&t1.id).await.unwrap().unwrap();
        assert_eq!(closed_child.status, "closed");
        assert_eq!(closed_child.close_reason.as_deref(), Some("parent_closed"));

        let _handle = spawn_coordinator(&db, &tx);
        // Negative assertion — same 400ms budget as the sibling tests above.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        let tasks = task_repo.list_by_epic(&epic.id).await.unwrap();
        assert_eq!(
            planning_count(&tasks),
            0,
            "closed epic should not trigger planning task"
        );
    }

    // ── AC4: Throughput tracking ───────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn throughput_snapshot_counts_recent_events() {
        let db = test_helpers::create_test_db();
        let (events_tx, _rx) = broadcast::channel::<DjinnEventEnvelope>(16);

        use crate::roles::RoleRegistry;
        use djinn_provider::catalog::health::HealthTracker;
        use djinn_slot::{ModelSlotConfig, SlotPoolConfig, SlotPoolHandle};

        let cancel = CancellationToken::new();
        let ctx = test_helpers::agent_context_from_db(db.clone(), cancel.clone());
        let pool = SlotPoolHandle::spawn(
            ctx,
            cancel.clone(),
            SlotPoolConfig {
                models: vec![ModelSlotConfig {
                    model_id: DEFAULT_MODEL_ID.to_owned(),
                    max_slots: 1,
                    roles: ["worker"].into_iter().map(ToOwned::to_owned).collect(),
                }],
                role_priorities: HashMap::new(),
            },
        );
        let (status_tx, _) = tokio::sync::watch::channel(SharedCoordinatorState {
            dispatched: 0,
            recovered: 0,
            epic_throughput: HashMap::new(),
            pr_errors: HashMap::new(),
            rate_limited_until: None,
        });
        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        let mut actor = CoordinatorActor::new(
            CoordinatorDeps::new(
                events_tx.clone(),
                cancel,
                db,
                pool,
                djinn_provider::catalog::CatalogService::new(),
                HealthTracker::new(),
                Arc::new(RoleRegistry::new()),
                BackgroundWorkTracker::default(),
                djinn_lsp::LspManager::new(),
            ),
            receiver,
            sender,
            status_tx,
        );

        // Record 3 events for epic "epic-1".
        actor.record_merge_event("epic-1");
        actor.record_merge_event("epic-1");
        actor.record_merge_event("epic-1");

        // Record 1 event for epic "epic-2".
        actor.record_merge_event("epic-2");

        let snap = actor.throughput_snapshot();
        assert_eq!(snap.get("epic-1"), Some(&3));
        assert_eq!(snap.get("epic-2"), Some(&1));
        assert_eq!(snap.get("epic-3"), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn throughput_evict_removes_old_events() {
        let db = test_helpers::create_test_db();
        let (events_tx, _rx) = broadcast::channel::<DjinnEventEnvelope>(16);

        use crate::roles::RoleRegistry;
        use djinn_provider::catalog::health::HealthTracker;
        use djinn_slot::{ModelSlotConfig, SlotPoolConfig, SlotPoolHandle};

        let cancel = CancellationToken::new();
        let ctx = test_helpers::agent_context_from_db(db.clone(), cancel.clone());
        let pool = SlotPoolHandle::spawn(
            ctx,
            cancel.clone(),
            SlotPoolConfig {
                models: vec![ModelSlotConfig {
                    model_id: DEFAULT_MODEL_ID.to_owned(),
                    max_slots: 1,
                    roles: ["worker"].into_iter().map(ToOwned::to_owned).collect(),
                }],
                role_priorities: HashMap::new(),
            },
        );
        let (status_tx, _) = tokio::sync::watch::channel(SharedCoordinatorState {
            dispatched: 0,
            recovered: 0,
            epic_throughput: HashMap::new(),
            pr_errors: HashMap::new(),
            rate_limited_until: None,
        });
        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        let mut actor = CoordinatorActor::new(
            CoordinatorDeps::new(
                events_tx.clone(),
                cancel,
                db,
                pool,
                djinn_provider::catalog::CatalogService::new(),
                HealthTracker::new(),
                Arc::new(RoleRegistry::new()),
                BackgroundWorkTracker::default(),
                djinn_lsp::LspManager::new(),
            ),
            receiver,
            sender,
            status_tx,
        );

        // Manually insert an expired event into the throughput map.
        actor
            .throughput_events
            .entry("epic-1".to_owned())
            .or_default()
            .push(StdInstant::now() - THROUGHPUT_WINDOW - Duration::from_secs(1));

        // Add a fresh event.
        actor.record_merge_event("epic-1");

        actor.evict_throughput_events();
        let snap = actor.throughput_snapshot();
        assert_eq!(
            snap.get("epic-1"),
            Some(&1),
            "expired events should be evicted"
        );
    }

    // ── ADR-051 §7: reentrance guard suppresses auto-dispatch ─────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn batch_completion_suppressed_when_active_planner_on_epic() {
        use djinn_db::{CreateSessionParams, SessionRepository};

        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let epic = make_epic(&db, &project.id, &tx).await;
        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        // One worker task plus a separate "host" task that the planner
        // session is attached to.  The host is a `review` task (non-worker,
        // non-planning) so it is excluded from both the worker-task count
        // and the open-planning-exists check — leaving the reentrance
        // guard's active-planner check as the ONLY thing that can suppress
        // dispatch.
        let t1 = create_task(&db, &epic.id, &project.id, "Task 1", "task", &tx).await;
        let planner_host =
            create_task(&db, &epic.id, &project.id, "Planner host", "review", &tx).await;

        // Insert a running planner session on `planner_host`.
        let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        session_repo
            .create(CreateSessionParams {
                project_id: &project.id,
                task_id: Some(&planner_host.id),
                model: "openai/gpt-5",
                agent_type: "planner",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();

        let _handle = spawn_coordinator(&db, &tx);

        // Closing the worker task should normally trigger batch-completion
        // auto-dispatch — but the active planner guard must suppress it.
        close_task(&db, &t1.id, &tx).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let tasks = task_repo.list_by_epic(&epic.id).await.unwrap();
        assert_eq!(
            planning_count(&tasks),
            0,
            "reentrance guard must suppress new planning task while planner is active"
        );
    }

    // ── Proposal review: incremental on epic close + backfill sweep ───────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_review_dispatched_incrementally_on_each_epic_close() {
        use djinn_db::{ProposalCreateInput, ProposalRepository};

        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let epic_repo = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let proposal_repo = ProposalRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        // A building proposal targeting `project`, with two graduated epics.
        let proposal = proposal_repo
            .create(ProposalCreateInput {
                title: "Closeout",
                body: "",
                acceptance_criteria: None,
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        proposal_repo
            .add_target(&proposal.id, &project.id, "primary")
            .await
            .unwrap();
        let e1 = make_epic(&db, &project.id, &tx).await;
        let e2 = make_epic(&db, &project.id, &tx).await;
        proposal_repo
            .link_epic(&proposal.id, &e1.id, &project.id)
            .await
            .unwrap();
        proposal_repo
            .link_epic(&proposal.id, &e2.id, &project.id)
            .await
            .unwrap();
        proposal_repo
            .set_building(&proposal.id, "user-x")
            .await
            .unwrap();

        let _handle = spawn_coordinator(&db, &tx);

        // Incremental: closing the FIRST epic already dispatches a review (so AC
        // progress can be reconciled as work lands), not only when all close.
        epic_repo.close(&e1.id).await.unwrap();
        assert_review_task_count(
            &task_repo,
            &project.id,
            &proposal.short_id,
            1,
            "closing a graduated epic should dispatch one proposal review task",
        )
        .await;

        // While that review is still open, neither a re-emitted close nor the
        // second epic closing stacks a duplicate (dedup: one open review).
        let closed_e1 = epic_repo.get(&e1.id).await.unwrap().unwrap();
        let _ = tx.send(DjinnEventEnvelope::epic_updated(&closed_e1));
        epic_repo.close(&e2.id).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert_eq!(
            review_task_count(&task_repo, &project.id, &proposal.short_id).await,
            1,
            "an open review must not be duplicated by further epic closes"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sweep_dispatches_review_for_drained_building_proposal() {
        use djinn_db::{ProposalCreateInput, ProposalRepository};

        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let epic_repo = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let proposal_repo = ProposalRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        // Build a drained building proposal WITHOUT a running coordinator, so no
        // event-driven review is created — mimicking a proposal whose epic closed
        // before the rule existed (e.g. `xoxg`).
        let proposal = proposal_repo
            .create(ProposalCreateInput {
                title: "Stranded",
                body: "",
                acceptance_criteria: None,
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        proposal_repo
            .add_target(&proposal.id, &project.id, "primary")
            .await
            .unwrap();
        let e1 = make_epic(&db, &project.id, &tx).await;
        proposal_repo
            .link_epic(&proposal.id, &e1.id, &project.id)
            .await
            .unwrap();
        proposal_repo
            .set_building(&proposal.id, "user-x")
            .await
            .unwrap();
        epic_repo.close(&e1.id).await.unwrap();

        assert_eq!(
            review_task_count(&task_repo, &project.id, &proposal.short_id).await,
            0,
            "precondition: no review task exists yet"
        );

        // The backfill sweep finds the drained proposal and dispatches a review.
        let actor = make_coordinator_actor(&db, &tx);
        actor.sweep_proposals_needing_review().await;
        assert_review_task_count(
            &task_repo,
            &project.id,
            &proposal.short_id,
            1,
            "backfill sweep should dispatch a review for the drained proposal",
        )
        .await;

        // Idempotent: a second sweep does not stack a duplicate.
        actor.sweep_proposals_needing_review().await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(
            review_task_count(&task_repo, &project.id, &proposal.short_id).await,
            1,
            "sweep must not duplicate an already-open review"
        );
    }

    // ── Proposal reconcile: amend event path + backfill sweep ────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn amend_while_building_dispatches_one_reconcile_task_from_event() {
        use djinn_db::{ProposalCreateInput, ProposalRepository, ProposalUpdateInput};

        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let proposal_repo = ProposalRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        let proposal = proposal_repo
            .create(ProposalCreateInput {
                title: "Live spec",
                body: "v1",
                acceptance_criteria: Some(r#"[{"criterion":"do X","met":false}]"#),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        proposal_repo
            .add_target(&proposal.id, &project.id, "primary")
            .await
            .unwrap();
        let epic = make_epic(&db, &project.id, &tx).await;
        proposal_repo
            .link_epic(&proposal.id, &epic.id, &project.id)
            .await
            .unwrap();
        let building = proposal_repo
            .set_building(&proposal.id, "user-x")
            .await
            .unwrap();

        let _handle = spawn_coordinator(&db, &tx);
        proposal_repo
            .update(
                &building.id,
                ProposalUpdateInput {
                    title: "Live spec v2",
                    body: "v2",
                    acceptance_criteria: r#"[{"criterion":"do X better","met":false}]"#,
                    status: "building",
                    superseded_by: None,
                    body_format: Some("markdown"),
                    event_metadata: None,
                },
            )
            .await
            .unwrap();

        assert_reconcile_task_count(
            &task_repo,
            &project.id,
            &building.short_id,
            1,
            "material amend while building should dispatch exactly one reconcile task",
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rapid_successive_building_amends_coalesce_to_one_open_reconcile_task() {
        use djinn_db::{ProposalCreateInput, ProposalRepository, ProposalUpdateInput};

        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let proposal_repo = ProposalRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        let proposal = proposal_repo
            .create(ProposalCreateInput {
                title: "Coalesce",
                body: "v1",
                acceptance_criteria: Some(r#"[{"criterion":"do X","met":false}]"#),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        proposal_repo
            .add_target(&proposal.id, &project.id, "primary")
            .await
            .unwrap();
        let epic = make_epic(&db, &project.id, &tx).await;
        proposal_repo
            .link_epic(&proposal.id, &epic.id, &project.id)
            .await
            .unwrap();
        let building = proposal_repo
            .set_building(&proposal.id, "user-x")
            .await
            .unwrap();

        let _handle = spawn_coordinator(&db, &tx);
        proposal_repo
            .update(
                &building.id,
                ProposalUpdateInput {
                    title: "Coalesce v2",
                    body: "v2",
                    acceptance_criteria: r#"[{"criterion":"do X v2","met":false}]"#,
                    status: "building",
                    superseded_by: None,
                    body_format: Some("markdown"),
                    event_metadata: None,
                },
            )
            .await
            .unwrap();
        proposal_repo
            .update(
                &building.id,
                ProposalUpdateInput {
                    title: "Coalesce v3",
                    body: "v3",
                    acceptance_criteria: r#"[{"criterion":"do X v3","met":false}]"#,
                    status: "building",
                    superseded_by: None,
                    body_format: Some("markdown"),
                    event_metadata: None,
                },
            )
            .await
            .unwrap();

        assert_reconcile_task_count(
            &task_repo,
            &project.id,
            &building.short_id,
            1,
            "rapid material amends must coalesce to one open reconcile task",
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert_eq!(
            reconcile_task_count(&task_repo, &project.id, &building.short_id).await,
            1,
            "an open reconcile task must not be duplicated by later amend events"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sweep_recovers_missed_building_amend_and_does_not_duplicate() {
        use djinn_db::{ProposalCreateInput, ProposalRepository, ProposalUpdateInput};

        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let proposal_repo = ProposalRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        let proposal = proposal_repo
            .create(ProposalCreateInput {
                title: "Missed event",
                body: "v1",
                acceptance_criteria: Some(r#"[{"criterion":"do X","met":false}]"#),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        proposal_repo
            .add_target(&proposal.id, &project.id, "primary")
            .await
            .unwrap();
        let epic = make_epic(&db, &project.id, &tx).await;
        proposal_repo
            .link_epic(&proposal.id, &epic.id, &project.id)
            .await
            .unwrap();
        let building = proposal_repo
            .set_building(&proposal.id, "user-x")
            .await
            .unwrap();

        // No coordinator is running for this material update, so the only way to
        // recover is the drift sweep.
        proposal_repo
            .update(
                &building.id,
                ProposalUpdateInput {
                    title: "Missed event v2",
                    body: "v2",
                    acceptance_criteria: r#"[{"criterion":"do X after downtime","met":false}]"#,
                    status: "building",
                    superseded_by: None,
                    body_format: Some("markdown"),
                    event_metadata: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            reconcile_task_count(&task_repo, &project.id, &building.short_id).await,
            0,
            "precondition: missed event created drift but no reconcile task"
        );

        let actor = make_coordinator_actor(&db, &tx);
        actor.sweep_proposals_needing_reconcile().await;
        assert_reconcile_task_count(
            &task_repo,
            &project.id,
            &building.short_id,
            1,
            "drift sweep should dispatch reconcile for missed building amend",
        )
        .await;

        actor.sweep_proposals_needing_reconcile().await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(
            reconcile_task_count(&task_repo, &project.id, &building.short_id).await,
            1,
            "second drift sweep must not duplicate an already-open reconcile task"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconcile_not_dispatched_for_non_building_or_zero_drift_proposals() {
        use djinn_db::{ProposalCreateInput, ProposalRepository, ProposalUpdateInput};

        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let proposal_repo = ProposalRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        let draft = proposal_repo
            .create(ProposalCreateInput {
                title: "Draft drift",
                body: "v1",
                acceptance_criteria: Some(r#"[{"criterion":"do X","met":false}]"#),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        proposal_repo
            .add_target(&draft.id, &project.id, "primary")
            .await
            .unwrap();
        proposal_repo
            .update(
                &draft.id,
                ProposalUpdateInput {
                    title: "Draft drift v2",
                    body: "v2",
                    acceptance_criteria: r#"[{"criterion":"do X maybe","met":false}]"#,
                    status: "draft",
                    superseded_by: None,
                    body_format: Some("markdown"),
                    event_metadata: None,
                },
            )
            .await
            .unwrap();

        let clean = proposal_repo
            .create(ProposalCreateInput {
                title: "Clean build",
                body: "v1",
                acceptance_criteria: Some(r#"[{"criterion":"do Y","met":false}]"#),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        proposal_repo
            .add_target(&clean.id, &project.id, "primary")
            .await
            .unwrap();
        let epic = make_epic(&db, &project.id, &tx).await;
        proposal_repo
            .link_epic(&clean.id, &epic.id, &project.id)
            .await
            .unwrap();
        let clean_building = proposal_repo
            .set_building(&clean.id, "user-x")
            .await
            .unwrap();

        let actor = make_coordinator_actor(&db, &tx);
        actor.sweep_proposals_needing_reconcile().await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(
            reconcile_task_count(&task_repo, &project.id, &draft.short_id).await,
            0,
            "non-building proposal drift must not dispatch reconcile"
        );
        assert_eq!(
            reconcile_task_count(&task_repo, &project.id, &clean_building.short_id).await,
            0,
            "building proposal without revision drift must not dispatch reconcile"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_review_not_dispatched_when_not_building() {
        use djinn_db::{ProposalCreateInput, ProposalRepository};

        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let epic_repo = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let proposal_repo = ProposalRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        // Proposal stays in draft (never graduated) but happens to have a linked
        // epic. Closing it must not trigger a review.
        let proposal = proposal_repo
            .create(ProposalCreateInput {
                title: "Draft",
                body: "",
                acceptance_criteria: None,
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        let e1 = make_epic(&db, &project.id, &tx).await;
        proposal_repo
            .link_epic(&proposal.id, &e1.id, &project.id)
            .await
            .unwrap();

        let _handle = spawn_coordinator(&db, &tx);
        epic_repo.close(&e1.id).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert_eq!(
            review_task_count(&task_repo, &project.id, &proposal.short_id).await,
            0,
            "a non-building proposal must not trigger a closeout review"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_reconcile_dispatches_once_for_building_revision_drift() {
        use djinn_db::{
            ProposalCreateInput, ProposalRepository, ProposalUpdateInput, UserRepository,
        };

        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let proposal_repo = ProposalRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let build_owner = UserRepository::new(db.clone())
            .upsert_from_github(42, "build-owner", None, None)
            .await
            .unwrap();

        let proposal = proposal_repo
            .create(ProposalCreateInput {
                title: "Amended Build",
                body: "original body",
                acceptance_criteria: None,
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        proposal_repo
            .add_target(&proposal.id, &project.id, "primary")
            .await
            .unwrap();
        let epic = make_epic(&db, &project.id, &tx).await;
        proposal_repo
            .link_epic(&proposal.id, &epic.id, &project.id)
            .await
            .unwrap();
        proposal_repo
            .set_building(&proposal.id, &build_owner.id)
            .await
            .unwrap();
        let drifted = proposal_repo
            .update(
                &proposal.id,
                ProposalUpdateInput {
                    title: "Amended Build",
                    body: "amended body",
                    acceptance_criteria: "[]",
                    status: "building",
                    superseded_by: None,
                    body_format: Some("markdown"),
                    event_metadata: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(drifted.last_reconciled_revision_seq, Some(1));
        assert_eq!(drifted.latest_revision_seq, 2);
        assert!(drifted.pending_reconcile);

        let mut actor = make_coordinator_actor(&db, &tx);
        actor
            .handle_event(DjinnEventEnvelope::proposal_updated(&drifted))
            .await;
        actor
            .dispatch_proposal_reconcile(&proposal_repo, &drifted, "proposal_updated")
            .await;

        assert_reconcile_task_count(
            &task_repo,
            &project.id,
            &proposal.short_id,
            1,
            "reconcile dispatch must dedupe while an open reconcile task exists",
        )
        .await;

        let tasks = task_repo.list_by_project(&project.id).await.unwrap();
        let task = tasks
            .iter()
            .find(|t| {
                t.title
                    == format!(
                        "{PROPOSAL_RECONCILE_TITLE_PREFIX} {}: {}",
                        proposal.short_id, proposal.title
                    )
            })
            .expect("reconcile task should exist");
        assert_eq!(task.issue_type.as_str(), "epic_breakdown");
        assert!(task.epic_id.is_none());
        assert_eq!(task.priority, PRIORITY_CRITICAL);
        assert_eq!(task.owner, "planner");
        assert_eq!(
            task.created_by_user_id.as_deref(),
            Some(build_owner.id.as_str())
        );
        assert!(task.design.contains("Single-flight / coalesce first"));
        assert!(task.design.contains("do **not** re-scope running tasks"));
        assert!(task.design.contains("proposal_reconcile_obsolete_epic"));
        assert!(
            task.design
                .contains("Do **not** use whole-build `proposal_stop_build`")
        );
        assert!(task.design.contains("merge_commit_sha IS NOT NULL"));
        assert!(task.design.contains("blocked response as terminal"));
        assert!(task.design.contains("preserve all state"));
        assert!(task.design.contains("leave unrelated epics untouched"));
        assert!(task.design.contains("proposal_ac_set"));
        assert!(task.design.contains("mark_reconciled"));

        let ac: Vec<serde_json::Value> = serde_json::from_str(&task.acceptance_criteria).unwrap();
        let ac_text = ac
            .iter()
            .filter_map(|c| c.get("criterion").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(ac_text.contains("proposal_reconcile_obsolete_epic"));
        assert!(ac_text.contains("blocked response"));
        assert!(ac_text.contains("leaves unrelated epics untouched"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_update_before_initial_breakdown_skips_reconcile() {
        use djinn_db::{
            ProposalCreateInput, ProposalRepository, ProposalUpdateInput, UserRepository,
        };

        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let proposal_repo = ProposalRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let build_owner = UserRepository::new(db.clone())
            .upsert_from_github(43, "initial-build-owner", None, None)
            .await
            .unwrap();

        let proposal = proposal_repo
            .create(ProposalCreateInput {
                title: "Initial Breakdown Drift",
                body: "original body",
                acceptance_criteria: None,
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        proposal_repo
            .add_target(&proposal.id, &project.id, "primary")
            .await
            .unwrap();

        let breakdown = task_repo
            .create_in_project(
                &project.id,
                None,
                &format!("Break down proposal: {}", proposal.title),
                "Call proposal_show before decomposing the proposal.",
                "Call proposal_show before decomposing the proposal.",
                IssueType::EpicBreakdown.as_str(),
                PRIORITY_CRITICAL,
                "planner",
                Some("open"),
                None,
            )
            .await
            .unwrap();
        proposal_repo
            .set_breakdown_task(&proposal.id, &breakdown.id)
            .await
            .unwrap();
        proposal_repo
            .set_building(&proposal.id, &build_owner.id)
            .await
            .unwrap();

        let drifted = proposal_repo
            .update(
                &proposal.id,
                ProposalUpdateInput {
                    title: "Initial Breakdown Drift",
                    body: "amended body before breakdown runs",
                    acceptance_criteria: "[]",
                    status: "building",
                    superseded_by: None,
                    body_format: Some("markdown"),
                    event_metadata: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(drifted.last_reconciled_revision_seq, Some(1));
        assert_eq!(drifted.latest_revision_seq, 2);
        assert!(drifted.pending_reconcile);
        assert!(
            proposal_repo
                .graduated_epics(&proposal.id)
                .await
                .unwrap()
                .is_empty()
        );

        let mut actor = make_coordinator_actor(&db, &tx);
        actor
            .handle_event(DjinnEventEnvelope::proposal_updated(&drifted))
            .await;
        actor
            .dispatch_proposal_reconcile(&proposal_repo, &drifted, "proposal_updated")
            .await;

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(
            reconcile_task_count(&task_repo, &project.id, &proposal.short_id).await,
            0,
            "proposal updates before any graduated epic must not spawn reconcile"
        );
        let open_breakdowns: Vec<_> = task_repo
            .list_by_project(&project.id)
            .await
            .unwrap()
            .into_iter()
            .filter(|t| {
                t.issue_type.as_str() == IssueType::EpicBreakdown.as_str()
                    && t.status != "closed"
                    && t.title == format!("Break down proposal: {}", proposal.title)
            })
            .collect();
        assert_eq!(
            open_breakdowns.len(),
            1,
            "only the original pending breakdown should remain open"
        );
        assert!(
            open_breakdowns[0].design.contains("proposal_show"),
            "pending initial breakdown still re-reads the proposal head when it runs"
        );

        let epic = make_epic(&db, &project.id, &tx).await;
        proposal_repo
            .link_epic(&proposal.id, &epic.id, &project.id)
            .await
            .unwrap();
        let reconciled = proposal_repo
            .get(&proposal.id)
            .await
            .unwrap()
            .expect("proposal should exist");
        assert_eq!(
            reconciled.last_reconciled_revision_seq,
            Some(drifted.latest_revision_seq)
        );
        assert!(!reconciled.pending_reconcile);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_updated_event_ignores_without_building_revision_drift() {
        use djinn_db::{ProposalCreateInput, ProposalRepository};

        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let proposal_repo = ProposalRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let proposal = proposal_repo
            .create(ProposalCreateInput {
                title: "No Drift",
                body: "body",
                acceptance_criteria: None,
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        proposal_repo
            .add_target(&proposal.id, &project.id, "primary")
            .await
            .unwrap();
        let building = proposal_repo
            .set_building(&proposal.id, "user-x")
            .await
            .unwrap();

        let mut actor = make_coordinator_actor(&db, &tx);
        actor
            .handle_event(DjinnEventEnvelope::proposal_updated(&building))
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert_eq!(
            reconcile_task_count(&task_repo, &project.id, &proposal.short_id).await,
            0,
            "building proposal without revision drift must not dispatch reconcile"
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn task_closed_event_records_linked_evidence_received_once() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let (proposal_repo, task_repo, proposal, spike_task, claim) =
            setup_linked_evidence_spike_fixture(&db, &tx, "Event Success").await;
        let findings = sample_evidence_findings("event-driven success");
        let findings_value = serde_json::to_value(&findings).unwrap();
        proposal_repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &proposal.id,
                kind: "evidence_findings",
                body: "valid event findings",
                blocking: false,
                agent_role: "spike",
                author_kind: "agent",
                author_model: None,
                source_task_id: Some(&spike_task.id),
                against_revision_seq: claim.against_revision_seq,
                round: claim.round,
                body_metadata: Some(&findings_value),
            })
            .await
            .unwrap();
        let closed = task_repo
            .set_status_with_reason(&spike_task.id, "closed", Some("completed"))
            .await
            .unwrap();

        let mut actor = make_coordinator_actor(&db, &tx);
        actor
            .handle_event(DjinnEventEnvelope::task_updated(&closed, false))
            .await;
        actor
            .handle_event(DjinnEventEnvelope::task_updated(&closed, false))
            .await;

        assert_eq!(
            count_evidence_lifecycle_events(
                &proposal_repo,
                &proposal.id,
                evidence_lifecycle_kind::EVIDENCE_RECEIVED,
            )
            .await,
            1,
            "duplicate closed-task events must remain idempotent"
        );
        assert_eq!(
            count_evidence_lifecycle_events(
                &proposal_repo,
                &proposal.id,
                evidence_lifecycle_kind::EVIDENCE_FAILED,
            )
            .await,
            0
        );
        let after_receipt = proposal_repo.get(&proposal.id).await.unwrap().unwrap();
        assert!(
            after_receipt.linked_spike_task_id.is_none(),
            "linked spike must be cleared after valid evidence receipt"
        );
        assert!(
            after_receipt.needs_evidence_claim.is_none(),
            "needs-evidence claim must be cleared after valid evidence receipt"
        );
        assert_eq!(tribunal_task_count(&task_repo, &closed.project_id).await, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn task_created_event_for_closed_linked_spike_records_evidence_received() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let (proposal_repo, task_repo, proposal, spike_task, claim) =
            setup_linked_evidence_spike_fixture(&db, &tx, "Event Created Success").await;
        let findings = sample_evidence_findings("event-created success");
        let findings_value = serde_json::to_value(&findings).unwrap();
        proposal_repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &proposal.id,
                kind: "evidence_findings",
                body: "valid event-created findings",
                blocking: false,
                agent_role: "spike",
                author_kind: "agent",
                author_model: None,
                source_task_id: Some(&spike_task.id),
                against_revision_seq: claim.against_revision_seq,
                round: claim.round,
                body_metadata: Some(&findings_value),
            })
            .await
            .unwrap();
        let closed = task_repo
            .set_status_with_reason(&spike_task.id, "closed", Some("completed"))
            .await
            .unwrap();

        let mut actor = make_coordinator_actor(&db, &tx);
        actor
            .handle_event(DjinnEventEnvelope::task_created(&closed, false))
            .await;

        assert_eq!(
            count_evidence_lifecycle_events(
                &proposal_repo,
                &proposal.id,
                evidence_lifecycle_kind::EVIDENCE_RECEIVED,
            )
            .await,
            1,
            "closed task_created events should follow the same linked-spike evidence path"
        );
        let after_receipt = proposal_repo.get(&proposal.id).await.unwrap().unwrap();
        assert!(
            after_receipt.linked_spike_task_id.is_none(),
            "linked spike must be cleared after valid evidence receipt via task_created event"
        );
        assert!(
            after_receipt.needs_evidence_claim.is_none(),
            "needs-evidence claim must be cleared after valid evidence receipt via task_created event"
        );
        assert_eq!(tribunal_task_count(&task_repo, &closed.project_id).await, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn task_closed_event_records_failed_for_failed_spike_and_blocks() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let (proposal_repo, task_repo, proposal, spike_task, _claim) =
            setup_linked_evidence_spike_fixture(&db, &tx, "Event Failed").await;
        let failed = task_repo
            .set_status_with_reason(&spike_task.id, "closed", Some("failed"))
            .await
            .unwrap();

        let mut actor = make_coordinator_actor(&db, &tx);
        actor
            .handle_event(DjinnEventEnvelope::task_updated(&failed, false))
            .await;

        assert_eq!(
            count_evidence_lifecycle_events(
                &proposal_repo,
                &proposal.id,
                evidence_lifecycle_kind::EVIDENCE_FAILED,
            )
            .await,
            1
        );
        let still_linked = proposal_repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(
            still_linked.linked_spike_task_id.as_deref(),
            Some(spike_task.id.as_str())
        );
        assert!(still_linked.needs_evidence_claim.is_some());
        assert_eq!(tribunal_task_count(&task_repo, &failed.project_id).await, 0);
    }

    // ── Startup recovery for terminal linked evidence spikes ─────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recover_terminal_linked_spike_evidence_records_received_and_clears_link() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let (proposal_repo, task_repo, proposal, spike_task, claim) =
            setup_linked_evidence_spike_fixture(&db, &tx, "Recovery Success").await;
        let findings = sample_evidence_findings("recovery success");
        let findings_value = serde_json::to_value(&findings).unwrap();
        proposal_repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &proposal.id,
                kind: "evidence_findings",
                body: "valid recovery findings",
                blocking: false,
                agent_role: "spike",
                author_kind: "agent",
                author_model: None,
                source_task_id: Some(&spike_task.id),
                against_revision_seq: claim.against_revision_seq,
                round: claim.round,
                body_metadata: Some(&findings_value),
            })
            .await
            .unwrap();
        task_repo
            .set_status_with_reason(&spike_task.id, "closed", Some("completed"))
            .await
            .unwrap();

        let mut actor = make_coordinator_actor(&db, &tx);
        actor.recover_terminal_linked_spike_evidence().await;

        assert_eq!(
            count_evidence_lifecycle_events(
                &proposal_repo,
                &proposal.id,
                evidence_lifecycle_kind::EVIDENCE_RECEIVED,
            )
            .await,
            1,
            "recovery must record exactly one refinement_evidence_received row"
        );
        assert_eq!(
            count_evidence_lifecycle_events(
                &proposal_repo,
                &proposal.id,
                evidence_lifecycle_kind::EVIDENCE_FAILED,
            )
            .await,
            0
        );
        // After successful recovery the durable evidence block is cleared
        // so the proposal is no longer parked on the spike.
        let recovered = proposal_repo.get(&proposal.id).await.unwrap().unwrap();
        assert!(
            recovered.linked_spike_task_id.is_none(),
            "linked_spike_task_id must be cleared after successful evidence recovery"
        );
        assert!(
            recovered.needs_evidence_claim.is_none(),
            "needs_evidence_claim must be cleared after successful evidence recovery"
        );
        assert_eq!(tribunal_task_count(&task_repo, &proposal.id).await, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recover_terminal_linked_spike_evidence_records_failed_for_terminal_failures() {
        for close_reason in ["failed", "cancelled", "force_closed"] {
            let db = test_helpers::create_test_db();
            let (tx, _rx) = broadcast::channel(256);
            let (proposal_repo, task_repo, proposal, spike_task, _claim) =
                setup_linked_evidence_spike_fixture(&db, &tx, "Recovery Failure").await;
            let status = if close_reason == "cancelled" {
                "cancelled"
            } else {
                "closed"
            };
            task_repo
                .set_status_with_reason(&spike_task.id, status, Some(close_reason))
                .await
                .unwrap();

            let mut actor = make_coordinator_actor(&db, &tx);
            actor.recover_terminal_linked_spike_evidence().await;

            assert_eq!(
                count_evidence_lifecycle_events(
                    &proposal_repo,
                    &proposal.id,
                    evidence_lifecycle_kind::EVIDENCE_FAILED,
                )
                .await,
                1,
                "recovery must record one failure for {close_reason}"
            );
            assert_eq!(
                count_evidence_lifecycle_events(
                    &proposal_repo,
                    &proposal.id,
                    evidence_lifecycle_kind::EVIDENCE_RECEIVED,
                )
                .await,
                0
            );
            let still_linked = proposal_repo.get(&proposal.id).await.unwrap().unwrap();
            assert_eq!(
                still_linked.linked_spike_task_id.as_deref(),
                Some(spike_task.id.as_str()),
                "proposal must remain linked/blocked after failure"
            );
            assert!(still_linked.needs_evidence_claim.is_some());
            assert_eq!(tribunal_task_count(&task_repo, &proposal.id).await, 0);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recover_terminal_linked_spike_evidence_records_failed_for_missing_or_malformed_findings()
     {
        for label in ["missing", "malformed"] {
            let db = test_helpers::create_test_db();
            let (tx, _rx) = broadcast::channel(256);
            let (proposal_repo, task_repo, proposal, spike_task, claim) =
                setup_linked_evidence_spike_fixture(&db, &tx, "Recovery No Findings").await;
            if label == "malformed" {
                // `add_debate_trail_entry` validates evidence findings, so a
                // malformed row can only exist as legacy data — write a valid
                // entry, then corrupt it underneath the repository API.
                let findings = sample_evidence_findings("recovery malformed");
                let findings_value = serde_json::to_value(&findings).unwrap();
                let entry = proposal_repo
                    .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                        proposal_id: &proposal.id,
                        kind: "evidence_findings",
                        body: "malformed recovery findings",
                        blocking: false,
                        agent_role: "spike",
                        author_kind: "agent",
                        author_model: None,
                        source_task_id: Some(&spike_task.id),
                        against_revision_seq: claim.against_revision_seq,
                        round: claim.round,
                        body_metadata: Some(&findings_value),
                    })
                    .await
                    .unwrap();
                let malformed = serde_json::json!({
                    "answer": "",
                    "evidence": [],
                    "code_paths_inspected": [],
                    "confidence": 0.7,
                    "residual_risks": [],
                    "recommendation_for_advocate": ""
                });
                djinn_db::test_support::override_debate_trail_body_metadata(
                    &db, &entry.id, &malformed,
                )
                .await;
            }
            task_repo
                .set_status_with_reason(&spike_task.id, "closed", Some("completed"))
                .await
                .unwrap();

            let mut actor = make_coordinator_actor(&db, &tx);
            actor.recover_terminal_linked_spike_evidence().await;

            assert_eq!(
                count_evidence_lifecycle_events(
                    &proposal_repo,
                    &proposal.id,
                    evidence_lifecycle_kind::EVIDENCE_FAILED,
                )
                .await,
                1,
                "recovery must record one failure for {label} findings"
            );
            assert_eq!(
                count_evidence_lifecycle_events(
                    &proposal_repo,
                    &proposal.id,
                    evidence_lifecycle_kind::EVIDENCE_RECEIVED,
                )
                .await,
                0
            );
            let still_linked = proposal_repo.get(&proposal.id).await.unwrap().unwrap();
            assert_eq!(
                still_linked.linked_spike_task_id.as_deref(),
                Some(spike_task.id.as_str())
            );
            assert!(still_linked.needs_evidence_claim.is_some());
            assert_eq!(tribunal_task_count(&task_repo, &proposal.id).await, 0);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recover_terminal_linked_spike_evidence_no_op_for_open_or_running_spikes() {
        for (status, label) in [("open", "open"), ("in_progress", "running")] {
            let db = test_helpers::create_test_db();
            let (tx, _rx) = broadcast::channel(256);
            let (proposal_repo, task_repo, proposal, spike_task, _claim) =
                setup_linked_evidence_spike_fixture(&db, &tx, "Recovery No-op").await;
            if status == "in_progress" {
                task_repo
                    .set_status_with_reason(&spike_task.id, status, None)
                    .await
                    .unwrap();
            }
            // "open" is the default status already set by the fixture.

            let mut actor = make_coordinator_actor(&db, &tx);
            actor.recover_terminal_linked_spike_evidence().await;

            assert_eq!(
                count_evidence_lifecycle_events(
                    &proposal_repo,
                    &proposal.id,
                    evidence_lifecycle_kind::EVIDENCE_RECEIVED,
                )
                .await,
                0,
                "{label} spikes must not produce receipt rows"
            );
            assert_eq!(
                count_evidence_lifecycle_events(
                    &proposal_repo,
                    &proposal.id,
                    evidence_lifecycle_kind::EVIDENCE_FAILED,
                )
                .await,
                0,
                "{label} spikes must not produce failure rows"
            );
            assert_eq!(tribunal_task_count(&task_repo, &proposal.id).await, 0);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recover_terminal_linked_spike_evidence_is_idempotent() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let (proposal_repo, task_repo, proposal, spike_task, claim) =
            setup_linked_evidence_spike_fixture(&db, &tx, "Recovery Idempotent").await;
        let findings = sample_evidence_findings("recovery idempotent");
        let findings_value = serde_json::to_value(&findings).unwrap();
        proposal_repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &proposal.id,
                kind: "evidence_findings",
                body: "valid idempotent findings",
                blocking: false,
                agent_role: "spike",
                author_kind: "agent",
                author_model: None,
                source_task_id: Some(&spike_task.id),
                against_revision_seq: claim.against_revision_seq,
                round: claim.round,
                body_metadata: Some(&findings_value),
            })
            .await
            .unwrap();
        task_repo
            .set_status_with_reason(&spike_task.id, "closed", Some("completed"))
            .await
            .unwrap();

        let mut actor = make_coordinator_actor(&db, &tx);
        actor.recover_terminal_linked_spike_evidence().await;

        // First pass records receipt and clears the link.
        let after_first = proposal_repo.get(&proposal.id).await.unwrap().unwrap();
        assert!(
            after_first.linked_spike_task_id.is_none(),
            "first recovery pass must clear the link"
        );

        // Second pass finds no candidates (link already cleared) so it is a
        // pure no-op — no duplicate lifecycle events or link mutations.
        actor.recover_terminal_linked_spike_evidence().await;

        assert_eq!(
            count_evidence_lifecycle_events(
                &proposal_repo,
                &proposal.id,
                evidence_lifecycle_kind::EVIDENCE_RECEIVED,
            )
            .await,
            1,
            "second recovery pass must not duplicate the receipt row"
        );
        assert_eq!(
            count_evidence_lifecycle_events(
                &proposal_repo,
                &proposal.id,
                evidence_lifecycle_kind::EVIDENCE_FAILED,
            )
            .await,
            0,
            "second recovery pass must not introduce a failure row"
        );
        let after_second = proposal_repo.get(&proposal.id).await.unwrap().unwrap();
        assert!(
            after_second.linked_spike_task_id.is_none(),
            "link must remain cleared after second pass"
        );
        assert_eq!(tribunal_task_count(&task_repo, &proposal.id).await, 0);
    }

    /// When the lifecycle event was already recorded by a prior recovery pass
    /// (or the event-driven path during startup) and the link was already
    /// cleared, recovery finds no candidates and is a no-op.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recover_terminal_linked_spike_evidence_no_candidates_when_link_already_cleared() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let (proposal_repo, task_repo, proposal, spike_task, claim) =
            setup_linked_evidence_spike_fixture(&db, &tx, "Already Cleared").await;
        let findings = sample_evidence_findings("already cleared");
        let findings_value = serde_json::to_value(&findings).unwrap();
        proposal_repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &proposal.id,
                kind: "evidence_findings",
                body: "valid already-cleared findings",
                blocking: false,
                agent_role: "spike",
                author_kind: "agent",
                author_model: None,
                source_task_id: Some(&spike_task.id),
                against_revision_seq: claim.against_revision_seq,
                round: claim.round,
                body_metadata: Some(&findings_value),
            })
            .await
            .unwrap();
        task_repo
            .set_status_with_reason(&spike_task.id, "closed", Some("completed"))
            .await
            .unwrap();

        // Simulate a prior pass that recorded the receipt AND cleared the link.
        let mut actor = make_coordinator_actor(&db, &tx);
        actor.recover_terminal_linked_spike_evidence().await;
        let after_prior = proposal_repo.get(&proposal.id).await.unwrap().unwrap();
        assert!(
            after_prior.linked_spike_task_id.is_none(),
            "prior pass must have cleared the link"
        );

        // A subsequent recovery pass finds no candidates (link is cleared)
        // and does not write duplicate lifecycle events.
        actor.recover_terminal_linked_spike_evidence().await;

        assert_eq!(
            count_evidence_lifecycle_events(
                &proposal_repo,
                &proposal.id,
                evidence_lifecycle_kind::EVIDENCE_RECEIVED,
            )
            .await,
            1,
            "subsequent pass must not duplicate the receipt"
        );
        assert_eq!(tribunal_task_count(&task_repo, &proposal.id).await, 0);
    }

    /// Recovery for a completed spike with valid findings that was already
    /// processed by the event-driven path (which recorded the receipt but
    /// did not clear the link — coordinator crashed between recording and
    /// calling resume). Recovery finds the lifecycle already recorded, still
    /// clears the link/claim via resume, and does not duplicate the receipt.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recover_terminal_linked_spike_evidence_clears_link_for_already_recorded() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let (proposal_repo, task_repo, proposal, spike_task, claim) =
            setup_linked_evidence_spike_fixture(&db, &tx, "Already Recorded").await;
        let findings = sample_evidence_findings("already recorded");
        let findings_value = serde_json::to_value(&findings).unwrap();
        proposal_repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &proposal.id,
                kind: "evidence_findings",
                body: "valid already-recorded findings",
                blocking: false,
                agent_role: "spike",
                author_kind: "agent",
                author_model: None,
                source_task_id: Some(&spike_task.id),
                against_revision_seq: claim.against_revision_seq,
                round: claim.round,
                body_metadata: Some(&findings_value),
            })
            .await
            .unwrap();
        task_repo
            .set_status_with_reason(&spike_task.id, "closed", Some("completed"))
            .await
            .unwrap();

        // Simulate the event-driven path recording the receipt first (without
        // clearing the link — as happens when the coordinator crashes between
        // recording the receipt and calling resume).
        proposal_repo
            .persist_terminal_linked_spike_evidence_lifecycle(
                &proposal.id,
                &spike_task.id,
                "closed",
                Some("completed"),
            )
            .await
            .unwrap();
        assert_eq!(
            count_evidence_lifecycle_events(
                &proposal_repo,
                &proposal.id,
                evidence_lifecycle_kind::EVIDENCE_RECEIVED,
            )
            .await,
            1,
            "setup: receipt must be recorded before recovery"
        );
        // Link is still set — the event-driven path was interrupted before
        // clearing it.
        let before_recovery = proposal_repo.get(&proposal.id).await.unwrap().unwrap();
        assert!(
            before_recovery.linked_spike_task_id.is_some(),
            "setup: link must still be set before recovery"
        );

        // Recovery runs and finds the lifecycle already recorded. It must
        // still clear the link/claim via resume.
        let mut actor = make_coordinator_actor(&db, &tx);
        actor.recover_terminal_linked_spike_evidence().await;

        let after_recovery = proposal_repo.get(&proposal.id).await.unwrap().unwrap();
        assert!(
            after_recovery.linked_spike_task_id.is_none(),
            "recovery must clear link even when lifecycle was already recorded"
        );
        assert!(
            after_recovery.needs_evidence_claim.is_none(),
            "recovery must clear claim even when lifecycle was already recorded"
        );
        assert_eq!(
            count_evidence_lifecycle_events(
                &proposal_repo,
                &proposal.id,
                evidence_lifecycle_kind::EVIDENCE_RECEIVED,
            )
            .await,
            1,
            "recovery must not duplicate the receipt row"
        );
        assert_eq!(tribunal_task_count(&task_repo, &proposal.id).await, 0);
    }
}
