// Proposal lifecycle tools: graduate, stop-build (abort/freeze/unfreeze),
// and scoped obsolete-epic reconciliation.
//
// This submodule owns:
// - `proposal_graduate` — kick off an approved proposal into the build engine;
// - `proposal_stop_build` — abort, freeze, or unfreeze an in-flight build;
// - `proposal_reconcile_obsolete_epic` — tear down one obsolete graduated epic
//   subtree without aborting the whole build;
// - private helpers `abort_proposal_build` and `teardown_obsolete_proposal_epic`
//   that compose the task/epic/session teardown cascade.
//
// Lifecycle tools call signoff/readiness helpers from `signoff.rs` (re-exported
// through `super`) via `evaluate_composed_gate`.  No readiness or debate logic
// is duplicated here — the composed gate evaluation is the single call-through
// point. Debate-trail blocking checks and refinement-stop authority are
// evaluated inside `evaluate_composed_gate` in signoff.rs (see that module's
// header for the co-location rationale).

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::server::DjinnMcpServer;
use crate::tools::acting_user::acting_caps;
use crate::tools::proposal_ops::{
    ProposalDispositionSummary, ProposalModel, ProposalReconcileObsoleteEpicResponse,
    ProposalSingleResponse,
};
use djinn_core::events::DjinnEventEnvelope;
use djinn_db::repositories::task::apply_parent_disposition_tx;
use djinn_db::{
    DispositionPlan, DispositionScope, EpicRepository, ProjectRepository, ProposalRepository,
    TaskRepository,
};

use super::{err_single, evaluate_composed_gate, proposal_not_found_error};

// ── Param structs ───────────────────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalGraduateParams {
    /// Proposal UUID or short_id (must be `approved`).
    pub id: String,
    /// Build owner — must be a participant (author or a sign-off giver).
    /// Defaults to the kicking-off user.
    pub owner_user_id: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalStopBuildParams {
    /// Proposal UUID or short_id (must be `building`).
    pub id: String,
    /// `abort` (tear the build down and revert to `approved`), `freeze` (hold
    /// the build's tasks out of dispatch, leaving epics/tasks/branches in
    /// place), or `unfreeze` (resume a frozen build).
    pub mode: String,
    /// Why the build is being stopped. Recorded with proposal terminal
    /// disposition activity. Required for `abort`.
    pub reason: Option<String>,
    /// When true on `abort`, classify linked epic children and return their
    /// disposition outcomes WITHOUT mutating anything.
    pub preview: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalReconcileObsoleteEpicParams {
    /// Proposal UUID or short_id. Alias for `proposal_id`.
    pub id: Option<String>,
    /// Proposal UUID or short_id. Alias for `id`.
    pub proposal_id: Option<String>,
    /// Epic UUID or short_id to retire from this proposal's graduated epics.
    pub epic_id: String,
    /// Why obsolete work is being reconciled. Defaults to a reconcile reason.
    pub reason: Option<String>,
    /// When true, classify disposition without closing/unlinking the epic.
    pub preview: Option<bool>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct ProposalStopBuildResponse {
    pub ok: bool,
    pub mode: String,
    pub proposal_id: Option<String>,
    /// Resulting proposal status (`approved` after a non-preview abort,
    /// `building` after freeze/unfreeze/preview).
    pub status: Option<String>,
    /// `true` when this was a dry-run that did not mutate anything.
    pub preview: bool,
    /// Epics torn down (abort) or that would be (preview).
    pub epics_closed: i64,
    /// Worker tasks disposed (closed) by the parent-disposition matrix.
    pub tasks_closed: i64,
    /// Retained for wire compatibility. Proposal disposition never kills
    /// sessions directly; normal status-change handling reconciles them.
    pub sessions_killed: i64,
    /// Child-disposition outcomes for all linked epics. Retention findings do
    /// not cause abort to fail.
    #[serde(default)]
    pub disposition: ProposalDispositionSummary,
    pub error: Option<String>,
}

// ── Lifecycle tool router ───────────────────────────────────────────────────

#[tool_router(router = proposal_lifecycle_tool_router, vis = "pub(super)")]
impl DjinnMcpServer {
    /// Kick off an approved proposal — graduate it into the execution engine.
    #[tool(
        description = "Kick off an approved proposal: hand it to the Planner (a single `epic_breakdown` task on the first primary target) which reads the spec + target repos and breaks it down into epics across the targets, set status to `building`, and record the build owner (must be a participant — the author or a sign-off giver; defaults to the caller). Requires the proposal to be `approved` and the engineer role (or admin)."
    )]
    pub async fn proposal_graduate(
        &self,
        Parameters(p): Parameters<ProposalGraduateParams>,
    ) -> Json<ProposalSingleResponse> {
        // Capability: engineer/admin only.
        match acting_caps(self.state.db()).await {
            Ok(Some(caps)) if !caps.can_kickoff() => {
                return Json(err_single(
                    "kicking off a build requires the engineer role (or admin)".to_string(),
                ));
            }
            Err(e) => return Json(err_single(e)),
            _ => {}
        }
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let task_repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());
        let project_repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());

        let Some(proposal) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(err_single(proposal_not_found_error(&p.id)));
        };
        if proposal.status != "approved" {
            return Json(err_single(format!(
                "proposal must be approved to kick off (current: {})",
                proposal.status
            )));
        }

        // Build owner must be a participant (author or sign-off giver).
        let participants = match repo.participants(&proposal.id).await {
            Ok(v) => v,
            Err(e) => return Json(err_single(e.to_string())),
        };
        let Some(owner) = p
            .owner_user_id
            .clone()
            .or_else(djinn_core::auth_context::current_user_id)
        else {
            return Json(err_single("a build owner is required".to_string()));
        };
        if !participants.is_empty() && !participants.contains(&owner) {
            return Json(err_single(
                "the build owner must be a participant (the author or a sign-off giver)"
                    .to_string(),
            ));
        }

        // At least one primary target is required — that is where the build
        // writes. The proposal-decomposition Planner reads the spec + targets
        // and creates the epics itself (cut over from the old mechanical
        // one-epic-per-primary fan-out).
        let targets = match repo.targets(&proposal.id).await {
            Ok(t) => t,
            Err(e) => return Json(err_single(e.to_string())),
        };
        let primaries: Vec<_> = targets.iter().filter(|t| t.role == "primary").collect();
        let Some(home_target) = primaries.first() else {
            return Json(err_single(
                "no primary target project to build — add a target first".to_string(),
            ));
        };
        let home_project_id = home_target.project_id.clone();

        // Composed gate (task cuzf): block graduation when DoR or tribunal
        // conditions are not met.  This prevents a malformed-but-approved
        // proposal from spawning a breakdown task, and also blocks when the
        // proposal is parked on a needs-evidence spike or blocked by
        // unresolved debate rows.  Earlier guardrails (capability, status,
        // build owner, primary target) already passed.
        {
            let gate = evaluate_composed_gate(
                &repo,
                &proposal,
                &proposal.body,
                &proposal.acceptance_criteria,
                targets.len(),
            )
            .await;
            if let Some(err) = gate.to_error_string() {
                return Json(err_single(err));
            }
        }

        // A human-readable target list for the breakdown task's design.
        let mut target_lines = Vec::with_capacity(targets.len());
        for t in &targets {
            let slug = match project_repo.get(&t.project_id).await {
                Ok(Some(proj)) => format!("{}/{}", proj.github_owner, proj.github_repo),
                _ => t.project_id.clone(),
            };
            target_lines.push(format!("- {slug} ({})", t.role));
        }

        let design = format!(
            "Decompose proposal `{}` ({}) into epics.\n\n\
             Call `proposal_show(id=\"{}\")` for the full spec, acceptance \
             criteria, and targets, then follow Workflow D (Proposal \
             Decomposition). Create one or more epics per `primary` target with \
             `epic_create(..., proposal_id=\"{}\")`, attach `reference` targets \
             as read-sources, and sequence cross-repo work with `blocked_by`. Do \
             NOT create worker tasks — each epic runs its own wave Planner.\n\n\
             Targets:\n{}",
            proposal.short_id,
            proposal.id,
            proposal.id,
            proposal.id,
            target_lines.join("\n"),
        );

        let ac = serde_json::json!([
            {"criterion": "Proposal read via proposal_show and target repos surveyed", "met": false},
            {"criterion": "One or more epics created per primary target (epic_create with proposal_id), with read_sources and blocked_by set for cross-repo ordering", "met": false},
            {"criterion": "submit_grooming called to finalize the breakdown", "met": false},
        ])
        .to_string();

        let title = format!("Break down proposal: {}", proposal.title);
        let task = match task_repo
            .create_in_project(
                &home_project_id,
                None,
                &title,
                &design,
                &design,
                "epic_breakdown",
                djinn_core::models::task::PRIORITY_CRITICAL,
                "planner",
                Some("open"),
                Some(&ac),
            )
            .await
        {
            Ok(t) => t,
            Err(e) => {
                return Json(err_single(format!("failed to create breakdown task: {e}")));
            }
        };
        // Attribute the breakdown task (and the epics the Planner spawns from it,
        // which inherit the session user) to the build owner so commits resolve
        // to a real GitHub account.
        let _ = task_repo.set_created_by_user_id(&task.id, &owner).await;
        // Record the breakdown task so a later proposal_stop_build can find and
        // force-close it (it has no epic, so it is not in proposal_epics).
        let _ = repo.set_breakdown_task(&proposal.id, &task.id).await;

        match repo.set_building(&proposal.id, &owner).await {
            Ok(updated) => Json(ProposalSingleResponse {
                proposal: Some(ProposalModel::from(&updated)),
                mdx: None,
                error: None,
            }),
            Err(e) => Json(err_single(e.to_string())),
        }
    }

    /// Stop an in-flight proposal build — the inverse of `proposal_graduate`.
    #[tool(
        description = "Stop an in-flight proposal build (mode: abort | freeze | unfreeze). Freeze and unfreeze only control dispatch. Abort uses the shared parent-disposition matrix across every linked epic: children are disposed (closed), parked for lead intervention, or retained when another open proposal parent or an external dependent requires it; it closes and unlinks linked epics and reverts the proposal to approved. Preview is read-only and reports the same disposition outcomes. Task status-change machinery reconciles workers, branches, and PRs."
    )]
    pub async fn proposal_stop_build(
        &self,
        Parameters(p): Parameters<ProposalStopBuildParams>,
    ) -> Json<ProposalStopBuildResponse> {
        let err = |msg: String| {
            Json(ProposalStopBuildResponse {
                ok: false,
                mode: String::new(),
                proposal_id: None,
                status: None,
                preview: false,
                epics_closed: 0,
                tasks_closed: 0,
                sessions_killed: 0,
                disposition: ProposalDispositionSummary::default(),
                error: Some(msg),
            })
        };

        // Capability: engineer/admin only (same gate as graduate — this is the
        // inverse build-control operation).
        match acting_caps(self.state.db()).await {
            Ok(Some(caps)) if !caps.can_kickoff() => {
                return err("stopping a build requires the engineer role (or admin)".to_string());
            }
            Err(e) => return err(e),
            _ => {}
        }

        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(proposal) = repo.resolve(&p.id).await.ok().flatten() else {
            return err(proposal_not_found_error(&p.id));
        };

        match p.mode.as_str() {
            "freeze" | "unfreeze" => {
                if proposal.status != "building" {
                    return err(format!(
                        "only a building proposal can be {}d (current: {})",
                        p.mode, proposal.status
                    ));
                }
                let frozen = p.mode == "freeze";
                match repo.set_frozen(&proposal.id, frozen).await {
                    Ok(updated) => Json(ProposalStopBuildResponse {
                        ok: true,
                        mode: p.mode,
                        proposal_id: Some(updated.id),
                        status: Some(updated.status),
                        preview: false,
                        epics_closed: 0,
                        tasks_closed: 0,
                        sessions_killed: 0,
                        disposition: ProposalDispositionSummary::default(),
                        error: None,
                    }),
                    Err(e) => err(e.to_string()),
                }
            }
            "abort" => {
                if proposal.status != "building" {
                    return err(format!(
                        "only a building proposal can be aborted (current: {})",
                        proposal.status
                    ));
                }
                let preview = p.preview.unwrap_or(false);
                let reason = p.reason.as_deref().unwrap_or("proposal build aborted");
                self.abort_proposal_build(&repo, &proposal, reason, preview)
                    .await
            }
            other => err(format!(
                "unknown mode '{other}' — expected abort | freeze | unfreeze"
            )),
        }
    }

    /// Tear down exactly one obsolete graduated epic subtree during proposal reconcile.
    #[tool(
        description = "Proposal reconcile helper: retire one obsolete graduated epic subtree without aborting the build. Pass `proposal_id` (or `id`) and `epic_id` (UUID or short_id). The tool verifies the epic is linked to the building proposal, blocks and records AI feedback if any target task has merged work, and supports `preview=true` for read-only blast radius. After the merged-work guard it applies the shared parent-disposition matrix to only the selected linked epic: target-epic children are disposed (closed), parked for lead intervention, or retained when another open proposal parent or an external dependent requires it; it closes and unlinks only that epic and leaves the proposal building with unrelated graduated epics linked. Preview is read-only and reports the same disposition outcomes. Task status-change machinery reconciles workers, branches, and PRs. Response includes ok, proposal_id, epic_id, preview, blocked/error, epics_closed, tasks_closed, disposition summary, and merged-work blocked details."
    )]
    pub async fn proposal_reconcile_obsolete_epic(
        &self,
        Parameters(p): Parameters<ProposalReconcileObsoleteEpicParams>,
    ) -> Json<ProposalReconcileObsoleteEpicResponse> {
        let err = |proposal_id: Option<String>, epic_id: Option<String>, msg: String| {
            Json(ProposalReconcileObsoleteEpicResponse {
                ok: false,
                proposal_id,
                epic_id,
                preview: p.preview.unwrap_or(false),
                blocked: false,
                blocked_feedback_id: None,
                blocked_feedback_body: None,
                merged_tasks: Vec::new(),
                epics_closed: 0,
                tasks_closed: 0,
                sessions_killed: 0,
                disposition: ProposalDispositionSummary::default(),
                error: Some(msg),
            })
        };

        match acting_caps(self.state.db()).await {
            Ok(Some(caps)) if !caps.can_kickoff() => {
                return err(
                    None,
                    None,
                    "reconciling obsolete epics requires the engineer role (or admin)".to_string(),
                );
            }
            Err(e) => return err(None, None, e),
            _ => {}
        }

        let Some(proposal_ref) = p.proposal_id.as_deref().or(p.id.as_deref()) else {
            return err(None, None, "missing proposal_id (or id)".to_string());
        };

        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(proposal) = repo.resolve(proposal_ref).await.ok().flatten() else {
            return err(None, None, proposal_not_found_error(proposal_ref));
        };
        let epic_repo = EpicRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(epic) = epic_repo.resolve(&p.epic_id).await.ok().flatten() else {
            return err(
                Some(proposal.id.clone()),
                None,
                format!("epic not found: {}", p.epic_id),
            );
        };

        let reason = p
            .reason
            .as_deref()
            .unwrap_or("proposal reconcile obsolete-subtree teardown");
        self.teardown_obsolete_proposal_epic(
            &repo,
            &proposal,
            &epic.id,
            reason,
            p.preview.unwrap_or(false),
        )
        .await
    }
}

// ── Build teardown (abort cascade) ─────────────────────────────────────────

impl DjinnMcpServer {
    /// Tear down a graduated build and revert the proposal to `approved`.
    ///
    /// One proposal-terminal scope covers every currently linked epic. The
    /// shared disposition primitive locks, classifies, changes, and audits its
    /// child tasks in the same transaction as the parent writes below.
    async fn abort_proposal_build(
        &self,
        repo: &ProposalRepository,
        proposal: &djinn_core::models::Proposal,
        _reason: &str,
        preview: bool,
    ) -> Json<ProposalStopBuildResponse> {
        let abort_err = |msg: String| {
            Json(ProposalStopBuildResponse {
                ok: false,
                mode: "abort".to_string(),
                proposal_id: Some(proposal.id.clone()),
                status: None,
                preview: false,
                epics_closed: 0,
                tasks_closed: 0,
                sessions_killed: 0,
                disposition: ProposalDispositionSummary::default(),
                error: Some(msg),
            })
        };
        let graduated = match repo.graduated_epics(&proposal.id).await {
            Ok(v) => v,
            Err(e) => return abort_err(e.to_string()),
        };
        let scope = DispositionScope::for_proposal_abort(
            &proposal.id,
            graduated.iter().map(|(id, _)| id.clone()).collect(),
        );
        let task_repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());
        if preview {
            let plan = match task_repo.classify_parent_disposition(&scope).await {
                Ok(plan) => plan,
                Err(e) => return abort_err(e.to_string()),
            };
            return Json(ProposalStopBuildResponse {
                ok: true,
                mode: "abort".to_string(),
                proposal_id: Some(proposal.id.clone()),
                status: Some(proposal.status.clone()),
                preview: true,
                epics_closed: graduated.len() as i64,
                tasks_closed: plan.counts.close as i64,
                sessions_killed: 0,
                disposition: proposal_disposition_summary(&plan),
                error: None,
            });
        }
        let mut tx = match self.state.db().pool().begin().await {
            Ok(tx) => tx,
            Err(e) => return abort_err(e.to_string()),
        };
        let plan = match apply_parent_disposition_tx(&mut tx, &scope).await {
            Ok(plan) => plan,
            Err(e) => return abort_err(e.to_string()),
        };
        let epic_ids = scope.epic_ids.clone();
        if let Err(e) = EpicRepository::close_scoped_tx(&mut tx, &epic_ids).await {
            return abort_err(e.to_string());
        }
        if let Err(e) = ProposalRepository::unlink_epics_tx(&mut tx, &proposal.id).await {
            return abort_err(e.to_string());
        }
        if let Err(e) = ProposalRepository::revert_to_approved_tx(&mut tx, &proposal.id).await {
            return abort_err(e.to_string());
        }
        if let Err(e) = tx.commit().await {
            return abort_err(e.to_string());
        }
        self.emit_disposition_events(&plan, &epic_ids).await;
        if let Ok(Some(updated)) = repo.get(&proposal.id).await {
            self.state
                .event_bus()
                .send(DjinnEventEnvelope::proposal_updated(&updated));
        }
        Json(ProposalStopBuildResponse {
            ok: true,
            mode: "abort".to_string(),
            proposal_id: Some(proposal.id.clone()),
            status: Some("approved".to_string()),
            preview: false,
            epics_closed: epic_ids.len() as i64,
            tasks_closed: plan.counts.close as i64,
            sessions_killed: 0,
            disposition: proposal_disposition_summary(&plan),
            error: None,
        })
    }

    /// Tear down exactly one obsolete graduated epic subtree for proposal reconcile.
    ///
    /// This is the scoped form of the abort cascade: it never touches the
    /// proposal build metadata, the breakdown task, or unrelated graduated epics.
    /// If any task in the target subtree has merged work, the operation records
    /// AI feedback and returns blocked before preview success or mutation.
    async fn teardown_obsolete_proposal_epic(
        &self,
        repo: &ProposalRepository,
        proposal: &djinn_core::models::Proposal,
        epic_id: &str,
        _reason: &str,
        preview: bool,
    ) -> Json<ProposalReconcileObsoleteEpicResponse> {
        let scoped_err = |msg: String| {
            Json(ProposalReconcileObsoleteEpicResponse {
                ok: false,
                proposal_id: Some(proposal.id.clone()),
                epic_id: Some(epic_id.to_string()),
                preview: false,
                blocked: false,
                blocked_feedback_id: None,
                blocked_feedback_body: None,
                merged_tasks: Vec::new(),
                epics_closed: 0,
                tasks_closed: 0,
                sessions_killed: 0,
                disposition: ProposalDispositionSummary::default(),
                error: Some(msg),
            })
        };

        if proposal.status != "building" {
            return scoped_err(format!(
                "only a building proposal can tear down an obsolete epic (current: {})",
                proposal.status
            ));
        }

        let graduated = match repo.graduated_epics(&proposal.id).await {
            Ok(v) => v,
            Err(e) => return scoped_err(e.to_string()),
        };
        if !graduated.iter().any(|(id, _)| id == epic_id) {
            return scoped_err(format!(
                "epic {epic_id} is not a graduated epic for proposal {}",
                proposal.id
            ));
        }

        let task_repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());

        let all_tasks = match task_repo.list_by_epic(epic_id).await {
            Ok(tasks) => tasks,
            Err(e) => return scoped_err(e.to_string()),
        };

        let merged_tasks: Vec<_> = all_tasks
            .iter()
            .filter(|t| t.merge_commit_sha.is_some())
            .collect();
        if !merged_tasks.is_empty() {
            let task_list = merged_tasks
                .iter()
                .map(|t| {
                    format!(
                        "- {} ({}) merged at {}",
                        t.short_id,
                        t.id,
                        t.merge_commit_sha.as_deref().unwrap_or("<unknown>")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            let body = format!(
                "Obsolete-subtree teardown is blocked for epic `{epic_id}` because it contains merged work. Preserve the subtree and reconcile manually before unlinking or closing it.\n\nMerged tasks:\n{task_list}"
            );
            let feedback = repo
                .add_feedback(djinn_db::ProposalFeedbackCreateInput {
                    proposal_id: &proposal.id,
                    parent_id: None,
                    author_kind: "ai",
                    author_model: Some("proposal-reconcile"),
                    body: &body,
                })
                .await;
            return Json(ProposalReconcileObsoleteEpicResponse {
                ok: false,
                proposal_id: Some(proposal.id.clone()),
                epic_id: Some(epic_id.to_string()),
                preview: false,
                blocked: true,
                blocked_feedback_id: feedback.as_ref().ok().map(|f| f.id.clone()),
                blocked_feedback_body: Some(body),
                merged_tasks: merged_tasks.iter().map(|t| t.id.clone()).collect(),
                epics_closed: 0,
                tasks_closed: 0,
                sessions_killed: 0,
                disposition: ProposalDispositionSummary::default(),
                error: Some(format!(
                    "obsolete epic teardown blocked by merged work in {} task(s)",
                    merged_tasks.len()
                )),
            });
        }

        let scope = DispositionScope::for_proposal_obsolete_epic_reconcile(&proposal.id, epic_id);
        let task_repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());
        if preview {
            let plan = match task_repo.classify_parent_disposition(&scope).await {
                Ok(plan) => plan,
                Err(e) => return scoped_err(e.to_string()),
            };
            return Json(ProposalReconcileObsoleteEpicResponse {
                ok: true,
                proposal_id: Some(proposal.id.clone()),
                epic_id: Some(epic_id.to_string()),
                preview: true,
                blocked: false,
                blocked_feedback_id: None,
                blocked_feedback_body: None,
                merged_tasks: Vec::new(),
                epics_closed: 1,
                tasks_closed: plan.counts.close as i64,
                sessions_killed: 0,
                disposition: proposal_disposition_summary(&plan),
                error: None,
            });
        }
        let mut tx = match self.state.db().pool().begin().await {
            Ok(tx) => tx,
            Err(e) => return scoped_err(e.to_string()),
        };
        let plan = match apply_parent_disposition_tx(&mut tx, &scope).await {
            Ok(plan) => plan,
            Err(e) => return scoped_err(e.to_string()),
        };
        if let Err(e) = EpicRepository::close_scoped_tx(&mut tx, &[epic_id.to_string()]).await {
            return scoped_err(e.to_string());
        }
        if let Err(e) = ProposalRepository::unlink_epic_tx(&mut tx, &proposal.id, epic_id).await {
            return scoped_err(e.to_string());
        }
        if let Err(e) = tx.commit().await {
            return scoped_err(e.to_string());
        }
        self.emit_disposition_events(&plan, &[epic_id.to_string()])
            .await;
        Json(ProposalReconcileObsoleteEpicResponse {
            ok: true,
            proposal_id: Some(proposal.id.clone()),
            epic_id: Some(epic_id.to_string()),
            preview: false,
            blocked: false,
            blocked_feedback_id: None,
            blocked_feedback_body: None,
            merged_tasks: Vec::new(),
            epics_closed: 1,
            tasks_closed: plan.counts.close as i64,
            sessions_killed: 0,
            disposition: proposal_disposition_summary(&plan),
            error: None,
        })
    }

    async fn emit_disposition_events(&self, plan: &DispositionPlan, epic_ids: &[String]) {
        let task_repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());
        for finding in plan
            .findings
            .iter()
            .filter(|finding| finding.disposition.applies_change())
        {
            if let Ok(Some(task)) = task_repo.get(&finding.task_id).await {
                self.state
                    .event_bus()
                    .send(DjinnEventEnvelope::task_updated(&task, false));
            }
        }
        let epic_repo = EpicRepository::new(self.state.db().clone(), self.state.event_bus());
        for epic_id in epic_ids {
            if let Ok(Some(epic)) = epic_repo.get(epic_id).await {
                self.state
                    .event_bus()
                    .send(DjinnEventEnvelope::epic_updated(&epic));
            }
        }
    }
}

fn proposal_disposition_summary(plan: &DispositionPlan) -> ProposalDispositionSummary {
    ProposalDispositionSummary {
        disposed: plan.counts.close as i64,
        parked: plan.counts.park as i64,
        retained_other_parent: plan.counts.retained_other_parent as i64,
        retained_external_dependent: plan.counts.retained_external_dependent as i64,
        retained_already_terminal: plan.counts.retained_already_terminal as i64,
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod stop_build_tests {
    use super::*;
    use crate::server::DjinnMcpServer;
    use crate::state::stubs::test_mcp_state;
    use djinn_core::events::EventBus;
    use djinn_db::{
        Database, EpicCreateInput, EpicRepository, ProjectRepository, ProposalCreateInput,
    };

    /// A `building` proposal: one graduated epic with two open worker tasks,
    /// plus a recorded breakdown task. The slot pool is the test stub (no live
    /// sessions), so the cascade exercises the DB-observable teardown.
    async fn building_proposal() -> (
        DjinnMcpServer,
        Database,
        String,
        String,
        Vec<String>,
        String,
    ) {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let bus = EventBus::noop();
        let project = ProjectRepository::new(db.clone(), bus.clone())
            .create("svc-stop", "test", "svc-stop")
            .await
            .unwrap();

        let prepo = ProposalRepository::new(db.clone(), bus.clone());
        let proposal = prepo
            .create(ProposalCreateInput {
                title: "Stop me",
                body: "",
                acceptance_criteria: None,
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let epic = EpicRepository::new(db.clone(), bus.clone())
            .create_for_project(
                &project.id,
                EpicCreateInput {
                    title: "E",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: None,
                    auto_breakdown: Some(false),
                    originating_adr_id: None,
                    blocked_by: None,
                },
            )
            .await
            .unwrap();

        let trepo = TaskRepository::new(db.clone(), bus.clone());
        let mut task_ids = Vec::new();
        for i in 0..2 {
            let t = trepo
                .create_in_project(
                    &project.id,
                    Some(&epic.id),
                    &format!("t{i}"),
                    "",
                    "",
                    "task",
                    0,
                    "",
                    Some("open"),
                    Some("[\"do\"]"),
                )
                .await
                .unwrap();
            task_ids.push(t.id);
        }
        let breakdown = trepo
            .create_in_project(
                &project.id,
                None,
                "breakdown",
                "",
                "",
                "epic_breakdown",
                0,
                "planner",
                Some("open"),
                None,
            )
            .await
            .unwrap();

        prepo
            .link_epic(&proposal.id, &epic.id, &project.id)
            .await
            .unwrap();
        prepo
            .set_breakdown_task(&proposal.id, &breakdown.id)
            .await
            .unwrap();
        prepo.set_building(&proposal.id, "owner").await.unwrap();

        (
            DjinnMcpServer::new(test_mcp_state(db.clone())),
            db,
            proposal.id,
            epic.id,
            task_ids,
            breakdown.id,
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn freeze_and_unfreeze_toggle_the_flag() {
        let (server, db, pid, _e, _t, _b) = building_proposal().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());

        let r = server
            .proposal_stop_build(Parameters(ProposalStopBuildParams {
                id: pid.clone(),
                mode: "freeze".into(),
                reason: None,
                preview: None,
            }))
            .await
            .0;
        assert!(r.ok);
        assert!(repo.get(&pid).await.unwrap().unwrap().build_frozen);

        let r = server
            .proposal_stop_build(Parameters(ProposalStopBuildParams {
                id: pid.clone(),
                mode: "unfreeze".into(),
                reason: None,
                preview: None,
            }))
            .await
            .0;
        assert!(r.ok);
        assert!(!repo.get(&pid).await.unwrap().unwrap().build_frozen);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preview_reports_blast_radius_without_mutating() {
        let (server, db, pid, epic_id, _t, _b) = building_proposal().await;
        let r = server
            .proposal_stop_build(Parameters(ProposalStopBuildParams {
                id: pid.clone(),
                mode: "abort".into(),
                reason: None,
                preview: Some(true),
            }))
            .await
            .0;
        assert!(r.ok);
        assert!(r.preview);
        assert_eq!(r.epics_closed, 1);
        assert_eq!(r.tasks_closed, 2, "only linked-epic children are disposed");

        // Nothing was mutated.
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        assert_eq!(repo.get(&pid).await.unwrap().unwrap().status, "building");
        let epic = EpicRepository::new(db.clone(), EventBus::noop())
            .get(&epic_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(epic.status, "open");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scoped_teardown_preview_reports_blast_radius_without_mutating() {
        let (server, db, pid, epic_id, task_ids, breakdown_id) = building_proposal().await;
        let r = server
            .dispatch_tool(
                "proposal_reconcile_obsolete_epic",
                serde_json::json!({
                    "proposal_id": pid.clone(),
                    "epic_id": epic_id.clone(),
                    "preview": true,
                }),
            )
            .await
            .unwrap();
        assert_eq!(r.get("ok").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(r.get("preview").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(r.get("epics_closed").and_then(|v| v.as_i64()), Some(1));
        assert_eq!(
            r.get("tasks_closed").and_then(|v| v.as_i64()),
            Some(task_ids.len() as i64)
        );

        let bus = EventBus::noop();
        let prepo = ProposalRepository::new(db.clone(), bus.clone());
        assert_eq!(prepo.get(&pid).await.unwrap().unwrap().status, "building");
        let links = prepo.graduated_epics(&pid).await.unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].0, epic_id);

        let erepo = EpicRepository::new(db.clone(), bus.clone());
        assert_eq!(erepo.get(&epic_id).await.unwrap().unwrap().status, "open");
        let trepo = TaskRepository::new(db.clone(), bus.clone());
        for tid in task_ids.iter().chain(std::iter::once(&breakdown_id)) {
            assert_eq!(trepo.get(tid).await.unwrap().unwrap().status, "open");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scoped_teardown_closes_only_target_epic_and_preserves_build() {
        let (server, db, pid, target_epic_id, target_task_ids, breakdown_id) =
            building_proposal().await;
        let bus = EventBus::noop();
        let project_repo = ProjectRepository::new(db.clone(), bus.clone());
        let project_id = project_repo
            .resolve("test/svc-stop")
            .await
            .unwrap()
            .unwrap();
        let project = project_repo.get(&project_id).await.unwrap().unwrap();
        let other_epic = EpicRepository::new(db.clone(), bus.clone())
            .create_for_project(
                &project.id,
                EpicCreateInput {
                    title: "Other",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: None,
                    auto_breakdown: Some(false),
                    originating_adr_id: None,
                    blocked_by: None,
                },
            )
            .await
            .unwrap();
        let trepo = TaskRepository::new(db.clone(), bus.clone());
        let other_task = trepo
            .create_in_project(
                &project.id,
                Some(&other_epic.id),
                "other-task",
                "",
                "",
                "task",
                0,
                "",
                Some("open"),
                Some("[\"do\"]"),
            )
            .await
            .unwrap();
        let prepo = ProposalRepository::new(db.clone(), bus.clone());
        prepo
            .link_epic(&pid, &other_epic.id, &project.id)
            .await
            .unwrap();
        let r = server
            .dispatch_tool(
                "proposal_reconcile_obsolete_epic",
                serde_json::json!({
                    "proposal_id": pid.clone(),
                    "epic_id": target_epic_id.clone(),
                    "reason": "obsolete after reconcile",
                }),
            )
            .await
            .unwrap();
        assert_eq!(r.get("ok").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(r.get("epics_closed").and_then(|v| v.as_i64()), Some(1));
        assert_eq!(
            r.get("tasks_closed").and_then(|v| v.as_i64()),
            Some(target_task_ids.len() as i64)
        );
        assert_eq!(r.get("blocked").and_then(|v| v.as_bool()), Some(false));

        let p = prepo.get(&pid).await.unwrap().unwrap();
        assert_eq!(p.status, "building");
        assert_eq!(
            p.build_breakdown_task_id.as_deref(),
            Some(breakdown_id.as_str())
        );
        assert_eq!(p.build_owner_user_id.as_deref(), Some("owner"));
        assert_eq!(
            prepo.graduated_epics(&pid).await.unwrap(),
            vec![(other_epic.id.clone(), project.id.clone())]
        );

        let erepo = EpicRepository::new(db.clone(), bus.clone());
        assert_eq!(
            erepo.get(&target_epic_id).await.unwrap().unwrap().status,
            "closed"
        );
        assert_eq!(
            erepo.get(&other_epic.id).await.unwrap().unwrap().status,
            "open"
        );

        for tid in &target_task_ids {
            assert_eq!(trepo.get(tid).await.unwrap().unwrap().status, "closed");
        }
        assert_eq!(
            trepo.get(&other_task.id).await.unwrap().unwrap().status,
            "open"
        );
        assert_eq!(
            trepo.get(&breakdown_id).await.unwrap().unwrap().status,
            "open"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scoped_teardown_blocks_merged_work_before_preview_or_mutation() {
        let (server, db, pid, target_epic_id, target_task_ids, breakdown_id) =
            building_proposal().await;
        let bus = EventBus::noop();
        let trepo = TaskRepository::new(db.clone(), bus.clone());
        trepo
            .set_merge_commit_sha(&target_task_ids[0], "abc123")
            .await
            .unwrap();
        let prepo = ProposalRepository::new(db.clone(), bus.clone());
        let r = server
            .dispatch_tool(
                "proposal_reconcile_obsolete_epic",
                serde_json::json!({
                    "id": pid.clone(),
                    "epic_id": target_epic_id.clone(),
                    "preview": true,
                }),
            )
            .await
            .unwrap();
        assert_eq!(r.get("ok").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(r.get("blocked").and_then(|v| v.as_bool()), Some(true));
        assert!(
            r.get("error")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .contains("blocked by merged work")
        );
        assert_eq!(r.get("epics_closed").and_then(|v| v.as_i64()), Some(0));
        assert_eq!(r.get("tasks_closed").and_then(|v| v.as_i64()), Some(0));
        assert!(
            r.get("blocked_feedback_body")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .contains("contains merged work")
        );

        let p = prepo.get(&pid).await.unwrap().unwrap();
        assert_eq!(p.status, "building");
        assert_eq!(
            p.build_breakdown_task_id.as_deref(),
            Some(breakdown_id.as_str())
        );
        assert_eq!(
            prepo.graduated_epics(&pid).await.unwrap(),
            vec![(
                target_epic_id.clone(),
                trepo
                    .get(&target_task_ids[0])
                    .await
                    .unwrap()
                    .unwrap()
                    .project_id
            )]
        );
        assert_eq!(
            EpicRepository::new(db.clone(), bus.clone())
                .get(&target_epic_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "open"
        );
        for tid in &target_task_ids {
            assert_eq!(trepo.get(tid).await.unwrap().unwrap().status, "open");
        }
        let feedback = prepo.feedback(&pid).await.unwrap();
        assert_eq!(feedback.len(), 1);
        assert_eq!(feedback[0].author_kind, "ai");
        assert!(feedback[0].body.contains("contains merged work"));
        assert!(feedback[0].body.contains(&target_task_ids[0]));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abort_tears_down_and_reverts_to_approved() {
        let (server, db, pid, epic_id, task_ids, breakdown_id) = building_proposal().await;
        let r = server
            .proposal_stop_build(Parameters(ProposalStopBuildParams {
                id: pid.clone(),
                mode: "abort".into(),
                reason: Some("changed my mind".into()),
                preview: Some(false),
            }))
            .await
            .0;
        assert!(r.ok, "abort failed: {:?}", r.error);
        assert_eq!(r.status.as_deref(), Some("approved"));
        assert_eq!(r.epics_closed, 1);
        assert_eq!(r.tasks_closed, 2);

        let bus = EventBus::noop();
        let prepo = ProposalRepository::new(db.clone(), bus.clone());
        let p = prepo.get(&pid).await.unwrap().unwrap();
        assert_eq!(p.status, "approved");
        assert!(p.build_owner_user_id.is_none());
        assert!(p.build_breakdown_task_id.is_none());
        assert!(prepo.graduated_epics(&pid).await.unwrap().is_empty());

        let epic = EpicRepository::new(db.clone(), bus.clone())
            .get(&epic_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(epic.status, "closed");

        let trepo = TaskRepository::new(db.clone(), bus.clone());
        for tid in &task_ids {
            let t = trepo.get(tid).await.unwrap().unwrap();
            assert_eq!(t.status, "closed", "linked task {tid} should be disposed");
        }
        assert_eq!(
            trepo.get(&breakdown_id).await.unwrap().unwrap().status,
            "open"
        );

        // A second abort is rejected — the proposal is no longer building.
        let r2 = server
            .proposal_stop_build(Parameters(ProposalStopBuildParams {
                id: pid,
                mode: "abort".into(),
                reason: Some("again".into()),
                preview: Some(false),
            }))
            .await
            .0;
        assert!(!r2.ok);
    }
}

// Graduation readiness tests — extracted to `graduation_readiness_tests.rs`
// to meet the 1500-line file-size guard. These tests cover `proposal_graduate`
// and its readiness guardrails, so they are paired with the lifecycle concern.
#[cfg(test)]
include!("graduation_readiness_tests.rs");
