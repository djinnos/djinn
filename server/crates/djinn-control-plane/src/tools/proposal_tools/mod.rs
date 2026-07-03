// MCP tools for the global Proposals layer (Phase 0).
//
// A proposal is a project-INDEPENDENT, collaboratively-authored artifact
// (spec body + acceptance criteria) that targets zero, one, or many projects
// via an editable M:N `proposal_targets` set. Discussion and suggestions share
// one `proposal_feedback` primitive (status == null → discussion; open/
// accepted/rejected → a trackable suggestion; author_kind == "ai" for future
// adversarial-review findings). Sign-offs gate approval, revisions/diffs track
// edits, and `proposal_graduate` kicks an approved proposal off into one epic
// per primary target (the existing single-repo-write execution engine).
//
// Submodule layout:
// - `create.rs` / `create_tests.rs`: CRUD/import/export/list/target tools
// - `feedback.rs`: feedback add/resolve tools
// - `signoff.rs` / `signoff_tests.rs`: signoff/clear tools and readiness/
//   composed-gate helpers (including debate-trail gate logic)
// - `mdx.rs`: MDX/block-patch parsing helpers
// - mod.rs: lifecycle tools (graduate, stop-build, reconcile), shared
//   response/error constructors, permission gates, and router composition

mod create;
pub(crate) mod feedback;
mod mdx;
pub(crate) mod signoff;

// Re-export CRUD tool parameter/response types so the public module path
// `crate::tools::proposal_tools::{...}` stays stable for existing dispatch and
// MCP-extension consumers.
pub use create::{
    ProposalCreateParams, ProposalDeleteParams, ProposalExportParams, ProposalImportParams,
    ProposalListParams, ProposalListResponse, ProposalShowParams, ProposalTargetParams,
    ProposalUpdateParams,
};

// Re-export feedback parameter types so `crate::tools::proposal_tools::...`
// stays stable for dispatch.rs and MCP-extension consumers.
pub use feedback::{ProposalFeedbackAddParams, ProposalFeedbackResolveParams};

// Re-export signoff parameter types so `crate::tools::proposal_tools::...`
// stays stable for dispatch.rs and MCP-extension consumers.
pub use signoff::ProposalSignoffParams;

// Re-export shared readiness/gate helpers from `signoff.rs` so `create.rs`
// (and later lifecycle slices) can continue importing from `super::*`.
pub(super) use signoff::{
    build_gate_status, evaluate_composed_gate, format_readiness_error, parse_ac_items,
};

// Re-export MDX/block-patch types so the public module path
// `crate::tools::proposal_tools::{...}` stays stable for existing dispatch
// and MCP-extension consumers.
pub use mdx::{
    BlockPatchOutcome, BlockPatchSelector, ByteRangeSelector, ProposalBlockPatchParams,
    apply_block_patch,
};

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::server::DjinnMcpServer;
use crate::tools::acting_user::acting_caps;
use crate::tools::proposal_ops::{
    ProposalModel, ProposalReconcileObsoleteEpicResponse, ProposalShowResponse,
    ProposalSingleResponse,
};
use djinn_db::{EpicRepository, ProjectRepository, ProposalRepository, TaskRepository};

pub(super) fn proposal_not_found_error(id: &str) -> String {
    format!("proposal not found: {id}")
}

// ── Param structs left in mod.rs for the follow-up slices. ────────────────────

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
    /// Why the build is being stopped. Recorded as the force-close reason on
    /// each torn-down task. Required for `abort`.
    pub reason: Option<String>,
    /// When true on `abort`, compute and return the blast radius (epics, open
    /// tasks, running sessions) WITHOUT mutating anything.
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
    /// Why obsolete work is being force-closed. Defaults to a reconcile teardown reason.
    pub reason: Option<String>,
    /// When true, compute blast radius without closing tasks, closing/unlinking the epic, or killing sessions.
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
    /// Worker tasks force-closed (abort) or open right now (preview).
    pub tasks_closed: i64,
    /// Running worker sessions killed (abort) or live now (preview).
    pub sessions_killed: i64,
    pub error: Option<String>,
}

// ── Tool router (remaining proposal tools; CRUD tools live in `create.rs`) ───

#[tool_router(router = proposal_remaining_tool_router, vis = "pub(super)")]
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
        description = "Stop an in-flight proposal build (mode: abort | freeze | unfreeze). `freeze` holds the build's tasks out of dispatch while leaving epics/tasks/branches in place; `unfreeze` resumes. `abort` tears the build down — kills running workers, force-closes every task (deleting branches so GitHub auto-closes their PRs), closes the epics, unlinks them, and reverts the proposal to `approved` so it can be edited and re-graduated. Pass preview=true with mode=abort for a read-only blast-radius (epics, open tasks, running sessions) without mutating. Requires the proposal to be `building` and the engineer role (or admin)."
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
        description = "Proposal reconcile helper: retire one obsolete graduated epic subtree without aborting the build. Pass `proposal_id` (or `id`) and `epic_id` (UUID or short_id). The tool verifies the epic is linked to the building proposal, blocks and records AI feedback if any target task has merged work, supports `preview=true` for read-only blast radius, and otherwise force-closes only target-epic tasks, kills their live sessions, closes/unlinks only that epic, and leaves the proposal building with unrelated graduated epics linked. Response includes ok, proposal_id, epic_id, preview, blocked/error, epics_closed, tasks_closed, sessions_killed, and merged-work blocked details."
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

// ── Build teardown (abort cascade) ───────────────────────────────────────────

impl DjinnMcpServer {
    /// Tear down a graduated build and revert the proposal to `approved`.
    ///
    /// Composes the existing teardown primitives, all idempotent and
    /// best-effort on side-effects: kill the running worker (pool), force-close
    /// each open task (`ForceClose` is exempt from the blocker guard, so order
    /// is irrelevant) and clean its branch/PR, close each graduated epic, unlink
    /// them so a re-graduation starts clean, and revert the proposal to
    /// `approved`. A `preview` returns the blast radius without mutating.
    async fn abort_proposal_build(
        &self,
        repo: &ProposalRepository,
        proposal: &djinn_core::models::Proposal,
        reason: &str,
        preview: bool,
    ) -> Json<ProposalStopBuildResponse> {
        use djinn_core::models::TransitionAction;

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
                error: Some(msg),
            })
        };

        let task_repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());
        let epic_repo = EpicRepository::new(self.state.db().clone(), self.state.event_bus());
        let pool = self.state.pool().await;

        let graduated = match repo.graduated_epics(&proposal.id).await {
            Ok(v) => v,
            Err(e) => return abort_err(e.to_string()),
        };

        // Every non-closed task across the graduated epics, plus the breakdown
        // task (which has no epic, so it is not in proposal_epics).
        let mut open_tasks: Vec<djinn_core::models::Task> = Vec::new();
        for (epic_id, _project_id) in &graduated {
            if let Ok(tasks) = task_repo.list_by_epic(epic_id).await {
                open_tasks.extend(tasks.into_iter().filter(|t| t.status != "closed"));
            }
        }
        if let Some(breakdown_id) = &proposal.build_breakdown_task_id
            && let Ok(Some(task)) = task_repo.resolve(breakdown_id).await
            && task.status != "closed"
        {
            open_tasks.push(task);
        }

        // Count live worker sessions (used by both preview and the kill count).
        let mut live_sessions = 0i64;
        if let Some(pool) = pool.as_ref() {
            for t in &open_tasks {
                if pool.has_session(&t.id).await.unwrap_or(false) {
                    live_sessions += 1;
                }
            }
        }

        if preview {
            return Json(ProposalStopBuildResponse {
                ok: true,
                mode: "abort".to_string(),
                proposal_id: Some(proposal.id.clone()),
                status: Some(proposal.status.clone()),
                preview: true,
                epics_closed: graduated.len() as i64,
                tasks_closed: open_tasks.len() as i64,
                sessions_killed: live_sessions,
                error: None,
            });
        }

        // Act. Kill the live worker (graceful), force-close the task, clean its
        // branch/PR. Best-effort throughout: a failed kill/cleanup is logged by
        // the underlying primitive and the next reaper backstops it.
        let mut sessions_killed = 0i64;
        let mut tasks_closed = 0i64;
        for t in &open_tasks {
            if let Some(pool) = pool.as_ref()
                && pool.has_session(&t.id).await.unwrap_or(false)
            {
                let _ = pool.kill_session(&t.id).await;
                sessions_killed += 1;
            }
            if task_repo
                .transition(
                    &t.id,
                    TransitionAction::ForceClose,
                    "system",
                    "system",
                    Some(reason),
                    None,
                )
                .await
                .is_ok()
            {
                tasks_closed += 1;
                self.state.cleanup_task_branches(&t.id).await;
            }
        }

        let mut epics_closed = 0i64;
        for (epic_id, _project_id) in &graduated {
            if epic_repo.close(epic_id).await.is_ok() {
                epics_closed += 1;
            }
        }

        let _ = repo.unlink_epics(&proposal.id).await;
        let status = match repo.revert_to_approved(&proposal.id).await {
            Ok(updated) => updated.status,
            Err(e) => return abort_err(e.to_string()),
        };

        Json(ProposalStopBuildResponse {
            ok: true,
            mode: "abort".to_string(),
            proposal_id: Some(proposal.id.clone()),
            status: Some(status),
            preview: false,
            epics_closed,
            tasks_closed,
            sessions_killed,
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
        reason: &str,
        preview: bool,
    ) -> Json<ProposalReconcileObsoleteEpicResponse> {
        use djinn_core::models::TransitionAction;

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
        let epic_repo = EpicRepository::new(self.state.db().clone(), self.state.event_bus());
        let pool = self.state.pool().await;

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
                error: Some(format!(
                    "obsolete epic teardown blocked by merged work in {} task(s)",
                    merged_tasks.len()
                )),
            });
        }

        let open_tasks: Vec<_> = all_tasks
            .into_iter()
            .filter(|t| t.status != "closed")
            .collect();

        let mut live_sessions = 0i64;
        if let Some(pool) = pool.as_ref() {
            for t in &open_tasks {
                if pool.has_session(&t.id).await.unwrap_or(false) {
                    live_sessions += 1;
                }
            }
        }

        if preview {
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
                tasks_closed: open_tasks.len() as i64,
                sessions_killed: live_sessions,
                error: None,
            });
        }

        let mut sessions_killed = 0i64;
        let mut tasks_closed = 0i64;
        for t in &open_tasks {
            if let Some(pool) = pool.as_ref()
                && pool.has_session(&t.id).await.unwrap_or(false)
            {
                let _ = pool.kill_session(&t.id).await;
                sessions_killed += 1;
            }
            if task_repo
                .transition(
                    &t.id,
                    TransitionAction::ForceClose,
                    "system",
                    "system",
                    Some(reason),
                    None,
                )
                .await
                .is_ok()
            {
                tasks_closed += 1;
                self.state.cleanup_task_branches(&t.id).await;
            }
        }

        let epics_closed = if epic_repo.close(epic_id).await.is_ok() {
            1
        } else {
            0
        };
        if let Err(e) = repo.unlink_epic(&proposal.id, epic_id).await {
            return scoped_err(e.to_string());
        }

        Json(ProposalReconcileObsoleteEpicResponse {
            ok: true,
            proposal_id: Some(proposal.id.clone()),
            epic_id: Some(epic_id.to_string()),
            preview: false,
            blocked: false,
            blocked_feedback_id: None,
            blocked_feedback_body: None,
            merged_tasks: Vec::new(),
            epics_closed,
            tasks_closed,
            sessions_killed,
            error: None,
        })
    }
}

// ── Permission gates ─────────────────────────────────────────────────────────

impl DjinnMcpServer {
    /// Gate a direct spec edit: allowed for the author, a PM, an engineer, or
    /// an admin. `Ok(())` when unauthenticated (trusted/system path).
    pub(crate) async fn gate_proposal_edit(
        &self,
        author_user_id: Option<&str>,
    ) -> Result<(), String> {
        if let Some(caps) = acting_caps(self.state.db()).await? {
            let is_author = author_user_id == Some(caps.user_id.as_str());
            if !caps.can_edit(is_author) {
                return Err(
                    "editing this proposal requires its author, a PM, an engineer, or an admin"
                        .to_string(),
                );
            }
        }
        Ok(())
    }
}

// ── Small response constructors (shared with `create.rs` via `pub(super)`) ───

pub(super) fn err_show(error: impl Into<String>) -> ProposalShowResponse {
    ProposalShowResponse {
        proposal: None,
        targets: None,
        feedback: None,
        revisions: None,
        signoffs: None,
        epics: None,
        memory_refs: vec![],
        debate_trail: None,
        refinement: None,
        gate_status: None,
        error: Some(error.into()),
    }
}

pub(super) fn err_single(error: impl Into<String>) -> ProposalSingleResponse {
    ProposalSingleResponse {
        proposal: None,
        mdx: None,
        error: Some(error.into()),
    }
}

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
        assert_eq!(r.tasks_closed, 3, "2 worker tasks + 1 breakdown task");

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
        assert_eq!(r.tasks_closed, 3);

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
        for tid in task_ids.iter().chain(std::iter::once(&breakdown_id)) {
            let t = trepo.get(tid).await.unwrap().unwrap();
            assert_eq!(t.status, "closed", "task {tid} should be force-closed");
        }

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

// ── Graduation readiness regression tests (task 9fjy) ───────────────────
//
// These tests gate-check the deterministic readiness evaluator wired into
// `proposal_graduate`.  An approved-but-malformed proposal must fail
// graduation with exact readiness details and leave the build state
// unchanged, while a complete approved proposal must graduate and create
// the breakdown planning task.

#[cfg(test)]
mod graduation_readiness_tests {
    use crate::server::DjinnMcpServer;
    use crate::state::stubs::test_mcp_state;
    use djinn_core::events::EventBus;
    use djinn_db::{
        Database, ProjectRepository, ProposalCreateInput, ProposalRepository, TaskRepository,
        UserRepository,
    };

    /// A well-formed body that passes all deterministic readiness checks.
    fn ready_body() -> &'static str {
        r#"
# Problem
Users cannot do X.

# Scope
In scope: Y. Out of scope: Z.

# Objectives
- Deliver A
- Deliver B

## File map
```file-map
    src/main.rs
    src/lib.rs
```

# Dependencies
Blocked by service C.

# Open Questions
What happens if D fails?
"#
    }

    /// A minimal body that fails most readiness checks (missing problem,
    /// scope, objectives, grounding, dependencies, open questions).
    fn incomplete_body() -> &'static str {
        "Just some random text without any required sections."
    }

    async fn setup_test_server_and_user() -> (DjinnMcpServer, Database, String) {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let user = UserRepository::new(db.clone())
            .upsert_from_github(999_800, "graduate-test-user", None, None)
            .await
            .unwrap();
        UserRepository::new(db.clone())
            .set_role(&user.id, "engineer")
            .await
            .unwrap();
        (DjinnMcpServer::new(test_mcp_state(db.clone())), db, user.id)
    }

    /// Advance a proposal directly to `approved` via SQL, simulating
    /// legacy data or a proposal that pre-dates the readiness gate.
    async fn force_approved(db: &Database, proposal_id: &str) {
        ProposalRepository::new(db.clone(), EventBus::noop())
            .set_status(proposal_id, "approved")
            .await
            .unwrap();
    }

    /// An approved proposal missing required readiness sections fails
    /// graduation with missing-check names in the error.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn approved_missing_sections_fails_graduation_with_check_names() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project = ProjectRepository::new(db.clone(), EventBus::noop())
            .create("svc-grad-missing", "test", "svc-grad-missing")
            .await
            .unwrap();

        let proposal = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                repo.create(ProposalCreateInput {
                    title: "Incomplete Graduation",
                    body: incomplete_body(),
                    acceptance_criteria: Some(r#"[{"criterion":"API returns 200","met":false}]"#),
                    status: None,
                    body_format: None,
                })
                .await
            })
            .await
            .unwrap();
        repo.add_target(&proposal.id, &project.id, "primary")
            .await
            .unwrap();

        force_approved(&db, &proposal.id).await;

        let response = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id), async {
                server
                    .dispatch_tool(
                        "proposal_graduate",
                        serde_json::json!({ "id": proposal.id }),
                    )
                    .await
            })
            .await
            .unwrap();

        let error = response.get("error").and_then(|v| v.as_str());
        assert!(
            error.is_some(),
            "expected readiness error, got: {response:?}"
        );
        let error = error.unwrap();
        assert!(
            error.contains("proposal not ready for review"),
            "error should mention readiness: {error}"
        );
        // At least some of the missing-section details should appear.
        assert!(
            error.contains("Missing required coverage"),
            "error should mention missing coverage: {error}"
        );

        let stored = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(stored.status, "approved");
        assert!(stored.build_breakdown_task_id.is_none());
        assert!(stored.build_owner_user_id.is_none());
    }

    /// A complete approved proposal graduates: the breakdown planning task
    /// is created, status moves to `building`, and build owner is set.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn complete_approved_proposal_graduates_and_creates_breakdown_task() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let task_repo = TaskRepository::new(db.clone(), EventBus::noop());
        let project = ProjectRepository::new(db.clone(), EventBus::noop())
            .create("svc-grad-ok", "test", "svc-grad-ok")
            .await
            .unwrap();

        let proposal = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                repo.create(ProposalCreateInput {
                    title: "Complete Graduation",
                    body: ready_body(),
                    acceptance_criteria: Some(r#"[{"criterion":"API returns 200","met":false}]"#),
                    status: None,
                    body_format: None,
                })
                .await
            })
            .await
            .unwrap();
        repo.add_target(&proposal.id, &project.id, "primary")
            .await
            .unwrap();

        force_approved(&db, &proposal.id).await;

        let response = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                server
                    .dispatch_tool(
                        "proposal_graduate",
                        serde_json::json!({ "id": proposal.id }),
                    )
                    .await
            })
            .await
            .unwrap();

        assert!(
            response.get("error").is_none(),
            "graduation should succeed: {:?}",
            response.get("error")
        );

        // Proposal must now be `building`.
        let stored = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(stored.status, "building", "status must advance to building");

        // Build owner must be the caller.
        assert_eq!(
            stored.build_owner_user_id.as_deref(),
            Some(user_id.as_str()),
            "build owner must be the caller"
        );

        // Breakdown task must be set.
        let breakdown_id = stored
            .build_breakdown_task_id
            .as_deref()
            .expect("breakdown task id must be set after graduation");
        let breakdown = task_repo
            .get(breakdown_id)
            .await
            .unwrap()
            .expect("breakdown task must exist");
        assert_eq!(breakdown.issue_type, "epic_breakdown");
        assert!(
            breakdown.title.contains("Complete Graduation"),
            "breakdown title must reference the proposal: {}",
            breakdown.title
        );
    }

    /// Regression: existing guardrails (capability, non-approved status)
    /// still fire before readiness.  A non-approved proposal fails
    /// graduation with the status guardrail error, not the readiness error.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn non_approved_proposal_fails_with_status_guardrail() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project = ProjectRepository::new(db.clone(), EventBus::noop())
            .create("svc-grad-status", "test", "svc-grad-status")
            .await
            .unwrap();

        let proposal = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                repo.create(ProposalCreateInput {
                    title: "Draft Proposal",
                    body: ready_body(),
                    acceptance_criteria: Some(r#"[{"criterion":"API returns 200","met":false}]"#),
                    status: None,
                    body_format: None,
                })
                .await
            })
            .await
            .unwrap();
        repo.add_target(&proposal.id, &project.id, "primary")
            .await
            .unwrap();

        // Do NOT advance to `approved` — the proposal is still `draft`.
        let response = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id), async {
                server
                    .dispatch_tool(
                        "proposal_graduate",
                        serde_json::json!({ "id": proposal.id }),
                    )
                    .await
            })
            .await
            .unwrap();

        let error = response.get("error").and_then(|v| v.as_str());
        assert!(error.is_some(), "expected error, got: {response:?}");
        let error = error.unwrap();
        // Must be the status guardrail, NOT the readiness error.
        assert!(
            error.contains("proposal must be approved"),
            "error must be the status guardrail: {error}"
        );
        assert!(
            !error.contains("proposal not ready"),
            "readiness must NOT mask the status guardrail: {error}"
        );
    }

    /// Regression: the no-primary-target guardrail still fires before
    /// readiness.  A proposal without targets fails with the target
    /// guardrail, not the readiness error.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_primary_target_fails_with_target_guardrail() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());

        let proposal = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                repo.create(ProposalCreateInput {
                    title: "No Target Proposal",
                    body: ready_body(),
                    acceptance_criteria: Some(r#"[{"criterion":"API returns 200","met":false}]"#),
                    status: None,
                    body_format: None,
                })
                .await
            })
            .await
            .unwrap();

        force_approved(&db, &proposal.id).await;

        // Do NOT add any target.
        let response = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id), async {
                server
                    .dispatch_tool(
                        "proposal_graduate",
                        serde_json::json!({ "id": proposal.id }),
                    )
                    .await
            })
            .await
            .unwrap();

        let error = response.get("error").and_then(|v| v.as_str());
        assert!(error.is_some(), "expected error, got: {response:?}");
        let error = error.unwrap();
        // Must be the primary-target guardrail, NOT the readiness error.
        assert!(
            error.contains("no primary target"),
            "error must be the primary-target guardrail: {error}"
        );
    }

    /// Lifecycle regression: the readiness error format is consistent
    /// across update (review promotion), sign-off, and graduation.
    /// Each path must surface the same "proposal not ready for review"
    /// preamble and missing-section / vague-AC detail structure.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn readiness_error_format_is_consistent_across_lifecycle_gates() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project = ProjectRepository::new(db.clone(), EventBus::noop())
            .create("svc-grad-format", "test", "svc-grad-format")
            .await
            .unwrap();

        // --- Update path: attempt to promote a draft to in_review ---
        let update_proposal = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                repo.create(ProposalCreateInput {
                    title: "Format Check Update",
                    body: incomplete_body(),
                    acceptance_criteria: Some(r#"[{"criterion":"API returns 200","met":false}]"#),
                    status: None,
                    body_format: None,
                })
                .await
            })
            .await
            .unwrap();
        repo.add_target(&update_proposal.id, &project.id, "primary")
            .await
            .unwrap();

        let update_resp = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                server
                    .dispatch_tool(
                        "proposal_update",
                        serde_json::json!({
                            "id": update_proposal.id,
                            "status": "in_review"
                        }),
                    )
                    .await
            })
            .await
            .unwrap();

        let update_err = update_resp.get("error").and_then(|v| v.as_str());
        assert!(update_err.is_some(), "update should fail: {update_resp:?}");
        let update_err = update_err.unwrap();
        assert!(
            update_err.starts_with("proposal not ready for review:"),
            "update error must start with readiness preamble: {update_err}"
        );

        // --- Sign-off path: attempt sign-off on a draft ---
        let signoff_proposal = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                repo.create(ProposalCreateInput {
                    title: "Format Check Signoff",
                    body: incomplete_body(),
                    acceptance_criteria: Some(r#"[{"criterion":"API returns 200","met":false}]"#),
                    status: None,
                    body_format: None,
                })
                .await
            })
            .await
            .unwrap();
        repo.add_target(&signoff_proposal.id, &project.id, "primary")
            .await
            .unwrap();

        let signoff_resp = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                server
                    .dispatch_tool(
                        "proposal_signoff",
                        serde_json::json!({
                            "id": signoff_proposal.id,
                            "kind": "technical"
                        }),
                    )
                    .await
            })
            .await
            .unwrap();

        let signoff_err = signoff_resp.get("error").and_then(|v| v.as_str());
        assert!(
            signoff_err.is_some(),
            "signoff should fail: {signoff_resp:?}"
        );
        let signoff_err = signoff_err.unwrap();
        assert!(
            signoff_err.starts_with("proposal not ready for review:"),
            "signoff error must start with readiness preamble: {signoff_err}"
        );

        // --- Graduation path: attempt graduation on an approved proposal ---
        let grad_proposal = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                repo.create(ProposalCreateInput {
                    title: "Format Check Graduate",
                    body: incomplete_body(),
                    acceptance_criteria: Some(r#"[{"criterion":"API returns 200","met":false}]"#),
                    status: None,
                    body_format: None,
                })
                .await
            })
            .await
            .unwrap();
        repo.add_target(&grad_proposal.id, &project.id, "primary")
            .await
            .unwrap();
        force_approved(&db, &grad_proposal.id).await;

        let grad_resp = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id), async {
                server
                    .dispatch_tool(
                        "proposal_graduate",
                        serde_json::json!({ "id": grad_proposal.id }),
                    )
                    .await
            })
            .await
            .unwrap();

        let grad_err = grad_resp.get("error").and_then(|v| v.as_str());
        assert!(grad_err.is_some(), "graduation should fail: {grad_resp:?}");
        let grad_err = grad_err.unwrap();
        assert!(
            grad_err.starts_with("proposal not ready for review:"),
            "graduation error must start with readiness preamble: {grad_err}"
        );

        // All three paths must use the same error format: they should all
        // contain the same set of missing-section details for this body.
        for err in [update_err, signoff_err, grad_err] {
            assert!(
                err.contains("Missing required coverage: problem"),
                "all gates must report missing problem: {err}"
            );
        }
    }
}

// ── End-to-end planner refinement loop regression (task iy6v) ────────────
//
// This module ties together the `y4td` surface delivered by the sibling tasks
// (1787 block-patch regressions, kepb planner prompt wiring, 18g4 patch
// primitive, 6al0 revision metadata, mzz8 schema-lean guard) into a single
// integrated regression that models the proposal `r0io` / `5bdd` flow:
//
//   1. A planner authoring session loads `visual-spec` from the native-skill
//      registry delivered by `5uzr` / `y8p2`.
//   2. The planner pulls `get_block_catalog` from the `ilqx` surface on demand
//      — block vocabulary is never inlined into prompts or write schemas.
//   3. The planner converts a markdown-only proposal draft into block-enriched
//      MDX through several targeted `proposal_block_patch` calls — never a
//      monolithic `proposal_update`.
//   4. Each patch records one proposal revision with `targeted_block_patch`
//      metadata and the active `visual-spec` version attribution.
//   5. The enriched proposal exports through `proposal_export` as valid MDX.
//
// Why these tests live here rather than as a separate cross-crate harness:
// the planner refinement loop is a property of how the control-plane MCP
// server stitches the surfaces together — `proposal_create`,
// `proposal_block_patch`, `proposal_show` (revisions), and `proposal_export`
// all run on the same `DjinnMcpServer` against a real `ProposalRepository`.
// The native-skill registry lookup and the block-catalog pull are pure-Rust
// surfaces that resolve at compile time.  This module therefore exercises
// the real delivered end-to-end surface without standing up the planner
// session runtime, which would require additional infrastructure.
#[cfg(test)]
mod end_to_end_planner_refinement_loop_tests {
    use super::super::proposal_blocks::{
        parse_mdx_blocks, proposal_block_catalog, validate_mdx_blocks,
    };
    use crate::server::DjinnMcpServer;
    use crate::state::stubs::test_mcp_state;
    use djinn_agent::native_skills::{native_skill_version, resolved_native_skills_for_role};
    use djinn_core::events::EventBus;
    use djinn_db::{Database, ProposalCreateInput, ProposalRepository};
    use serde_json::Value;

    async fn test_server() -> (DjinnMcpServer, Database) {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        (DjinnMcpServer::new(test_mcp_state(db.clone())), db)
    }

    /// Multi-section markdown proposal draft used as the starting point for
    /// the planner refinement loop.  Four independently targetable sections /
    /// paragraphs are present so the test exercises the `proposal_block_patch`
    /// primitive over multiple distinct selectors.
    const DRAFT_BODY: &str = "\
# Visual-spec authoring integration

The opening paragraph introduces the proposal and explains its purpose.

## Approach

The approach section describes the high-level plan in prose.

## Tradeoffs

The tradeoffs section enumerates the costs of the chosen approach.

## Open Questions

The open-questions section collects uncertainties for the team.
";

    /// AC: planner authoring sessions receive the native `visual-spec` skill
    /// (delivered by `y8p2`) through the resolved-native-skills surface so the
    /// planner can `skill_read` it on demand rather than embedding it in the
    /// prompt body.  This is the lazy loading contract that lets
    /// non-authoring planner sessions avoid paying the visual-spec body cost.
    #[test]
    fn planner_authoring_session_resolves_visual_spec_from_native_registry() {
        // A planner authoring session must resolve exactly one native skill
        // — `visual-spec` — through the registry exposed by `y8p2`.
        let resolved = resolved_native_skills_for_role("planner");
        let names: Vec<&str> = resolved.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"visual-spec"),
            "planner authoring session must resolve visual-spec via the native \
             registry; got {names:?}"
        );

        // The version stamp must come from the same registry (no parallel
        // version source) so the planner can pass it through
        // `proposal_block_patch` for revision attribution.  `ResolvedSkill`
        // does not carry `version` (that field is reserved for the immutable
        // native registry), so we read the version from
        // `native_skill_version` directly.
        let registry_version = native_skill_version("visual-spec")
            .expect("native_skill_version must return the active visual-spec version");
        assert!(
            !registry_version.is_empty(),
            "native_skill_version must return a non-empty version stamp"
        );

        // The resolved skill must be marked `required: true` for the planner
        // role — the planner can't author MDX without it.  This pins the
        // lazy-loading contract: the registry is the single source of truth
        // for the active version, not a duplicated constant in prompts.
        let visual_spec = resolved.iter().find(|s| s.name == "visual-spec").unwrap();
        assert!(
            visual_spec.required,
            "visual-spec must be required for the planner authoring session"
        );
    }

    /// AC: non-authoring sessions (e.g. `worker`, `reviewer`) must NOT
    /// receive `visual-spec`.  This is the lazy-loading guard: only the
    /// planner role pays the visual-spec body cost.
    #[test]
    fn non_authoring_sessions_do_not_receive_visual_spec() {
        for role in ["worker", "reviewer"] {
            let resolved = resolved_native_skills_for_role(role);
            let names: Vec<&str> = resolved.iter().map(|s| s.name.as_str()).collect();
            assert!(
                !names.contains(&"visual-spec"),
                "{role} session must not receive visual-spec; got {names:?}"
            );
        }
    }

    /// AC: the `get_block_catalog` pull surface (delivered by `ilqx`) returns
    /// the lean (type, tag) vocabulary the planner uses to discover block
    /// names without inlining them in prompts or write schemas.  This test
    /// pins the catalog against the registry so a future drift between the
    /// two surfaces is caught.
    #[test]
    fn get_block_catalog_pull_surface_returns_lean_vocabulary() {
        let catalog = proposal_block_catalog();
        assert_eq!(
            catalog.len(),
            14,
            "get_block_catalog must expose all 14 v1 block types so the planner \
             can discover the vocabulary on demand"
        );
        // The catalog is the lean projection — no field schemas, no
        // descriptions, just (type, tag) pairs.  Any future regression that
        // bloats the catalog back into the rich registry shape is caught
        // here.
        for entry in &catalog {
            assert!(
                !entry.block_type.is_empty(),
                "catalog entry has empty type: {entry:?}"
            );
            assert!(
                !entry.tag.is_empty(),
                "catalog entry has empty tag: {entry:?}"
            );
        }
    }

    /// AC: a markdown-only proposal draft is progressively enriched into
    /// `body_format=mdx` through the targeted `proposal_block_patch`
    /// primitive.  `latest_revision_seq` must equal the seed revision (1)
    /// plus the number of patches applied.  A monolithic whole-body
    /// `proposal_update` is forbidden for enrichment — the test only
    /// invokes `proposal_block_patch`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_loop_increments_revision_seq_once_per_targeted_patch() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Planner Refinement Loop",
                body: DRAFT_BODY,
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        assert_eq!(
            proposal.latest_revision_seq, 1,
            "create seed must leave latest_revision_seq at 1"
        );
        assert_eq!(
            proposal.body_format, "markdown",
            "create seed must produce body_format=markdown"
        );

        // Three independently targetable sections get promoted to MDX blocks
        // through three sequential `proposal_block_patch` calls.  Each one
        // is exactly one material proposal edit (= one revision increment).
        let visual_spec_version = native_skill_version("visual-spec")
            .expect("native_skill_version must return the active visual-spec version");

        let patches = [
            (
                "The opening paragraph introduces the proposal and explains its purpose.",
                "<RichText id=\"opening\">\nThe structured opening paragraph.\n</RichText>",
                "first patch: opening paragraph -> RichText",
            ),
            (
                "The approach section describes the high-level plan in prose.",
                "<FileTree id=\"repo-layout\" name=\"repo\" />",
                "second patch: approach -> FileTree",
            ),
            (
                "The tradeoffs section enumerates the costs of the chosen approach.",
                "<Callout id=\"tradeoffs-callout\">\nThe structured tradeoff callout.\n</Callout>",
                "third patch: tradeoffs -> Callout",
            ),
        ];

        let mut expected_seq: i32 = 1;
        let mut prev_expected_revision_seq: Option<i32> = None;
        for (selector_text, block_mdx, note) in patches {
            let mut args = serde_json::json!({
                "id": proposal.id,
                "selector": { "exact_text": selector_text },
                "operation": "replace",
                "block_mdx": block_mdx,
                "native_skill_name": "visual-spec",
                "native_skill_version": visual_spec_version,
                "note": note,
            });
            // Pass `expected_latest_revision_seq` to exercise the stale-revision
            // guard path the prompt wires up for sequential patches.
            if let Some(prev) = prev_expected_revision_seq {
                args["expected_latest_revision_seq"] = serde_json::json!(prev);
            }

            let response = server
                .dispatch_tool("proposal_block_patch", args)
                .await
                .unwrap();
            assert!(
                response.get("error").is_none(),
                "proposal_block_patch failed for {note:?}: {:?}",
                response.get("error")
            );
            expected_seq += 1;
            prev_expected_revision_seq = Some(expected_seq);

            let after = repo.get(&proposal.id).await.unwrap().unwrap();
            assert_eq!(
                after.latest_revision_seq, expected_seq,
                "latest_revision_seq must be exactly +1 per patch after {note:?}"
            );
        }

        // Final state: three patches landed -> latest_revision_seq = 4
        // (1 seed + 3 patches).
        let final_state = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(
            final_state.latest_revision_seq, 4,
            "three patches from a 1-seed proposal must yield latest_revision_seq=4"
        );
        assert_eq!(
            final_state.body_format, "mdx",
            "first MDX block patch must upgrade body_format to mdx"
        );

        // The proposal_show surface must report the same revision seq
        // (drift between the repo state and the public surface is caught here).
        let shown = server
            .dispatch_tool("proposal_show", serde_json::json!({ "id": proposal.id }))
            .await
            .unwrap();
        assert_eq!(
            shown
                .get("proposal")
                .and_then(|p| p.get("latest_revision_seq"))
                .and_then(|v| v.as_i64()),
            Some(4),
            "proposal_show.proposal.latest_revision_seq must match the repo state"
        );
    }

    /// AC: revision history exposes `targeted_block_patch` metadata on every
    /// patch revision, including the active `visual-spec` native-skill
    /// version attribution.  The seed revision must NOT carry that metadata
    /// — that signal is reserved for the patch primitive.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_loop_revisions_carry_visual_spec_attribution() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Refinement Loop Attribution",
                body: DRAFT_BODY,
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let visual_spec_version = native_skill_version("visual-spec")
            .expect("native_skill_version must return the active visual-spec version");

        // Apply two patches, attributing each to the registry's active version.
        for (selector_text, block_mdx) in [
            (
                "The opening paragraph introduces the proposal and explains its purpose.",
                "<RichText id=\"opening\">\nStructured opening.\n</RichText>",
            ),
            (
                "The approach section describes the high-level plan in prose.",
                "<FileTree id=\"repo\" name=\"repo\" />",
            ),
        ] {
            let response = server
                .dispatch_tool(
                    "proposal_block_patch",
                    serde_json::json!({
                        "id": proposal.id,
                        "selector": { "exact_text": selector_text },
                        "operation": "replace",
                        "block_mdx": block_mdx,
                        "native_skill_name": "visual-spec",
                        "native_skill_version": visual_spec_version,
                    }),
                )
                .await
                .unwrap();
            assert!(
                response.get("error").is_none(),
                "patch failed: {:?}",
                response.get("error")
            );
        }

        // Walk revisions through proposal_show — the surface the planner
        // consumes — and assert metadata shape on every patch revision.
        let shown = server
            .dispatch_tool("proposal_show", serde_json::json!({ "id": proposal.id }))
            .await
            .unwrap();
        let revisions = shown
            .get("revisions")
            .and_then(|v| v.as_array())
            .expect("proposal_show.revisions must be a JSON array");

        // 1 seed + 2 patches = 3 revisions.
        assert_eq!(
            revisions.len(),
            3,
            "expected 3 revisions (1 seed + 2 patches); got {}",
            revisions.len()
        );

        // The seed revision must NOT carry targeted-block-patch metadata —
        // `proposal_create` writes no event_metadata.
        let seed = &revisions[0];
        let seed_meta = seed.get("event_metadata");
        assert!(
            seed_meta.is_none() || seed_meta.is_some_and(|v| v.is_null()),
            "create seed revision must not carry event_metadata, got {seed_meta:?}"
        );

        // Every patch revision must carry the targeted-block-patch signal
        // AND the active `visual-spec` version from the native registry.
        for (idx, rev) in revisions.iter().enumerate().skip(1) {
            let meta_str = rev
                .get("event_metadata")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!("patch rev #{idx} must expose event_metadata"));
            let meta: Value = serde_json::from_str(meta_str)
                .unwrap_or_else(|e| panic!("patch rev #{idx} event_metadata must be JSON: {e}"));
            assert_eq!(
                meta["change_kind"], "targeted_block_patch",
                "patch rev #{idx} must identify as targeted_block_patch"
            );
            assert_eq!(
                meta["native_skill_name"], "visual-spec",
                "patch rev #{idx} must attribute the native skill name"
            );
            assert_eq!(
                meta["native_skill_version"], visual_spec_version,
                "patch rev #{idx} must attribute the active visual-spec version from \
                 the native registry (drift between registry and patch metadata is \
                 caught here)"
            );
            // The byte-range fields are present and well-typed.
            assert!(
                meta["range_start_byte"].is_number() && meta["range_end_byte"].is_number(),
                "patch rev #{idx} must expose numeric byte-range fields"
            );
            assert!(
                meta["range_end_byte"].as_u64().unwrap()
                    > meta["range_start_byte"].as_u64().unwrap(),
                "patch rev #{idx} range_end_byte must exceed range_start_byte"
            );
        }
    }

    /// AC: after the refinement loop, the proposal exports cleanly through
    /// `proposal_export` and the returned MDX round-trips through the block
    /// parser.  This is the end-to-end fidelity contract that ties the
    /// refinement loop back to the MDX export surface.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_loop_enriched_proposal_exports_as_valid_mdx() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Refinement Loop Export",
                body: DRAFT_BODY,
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let visual_spec_version = native_skill_version("visual-spec")
            .expect("native_skill_version must return the active visual-spec version");

        // Apply three sequential targeted patches — the full planner refinement
        // loop from a markdown-only draft.
        for (selector_text, block_mdx) in [
            (
                "The opening paragraph introduces the proposal and explains its purpose.",
                "<RichText id=\"opening\">\nStructured opening.\n</RichText>",
            ),
            (
                "The approach section describes the high-level plan in prose.",
                "<FileTree id=\"repo-layout\" name=\"repo\" />",
            ),
            (
                "The tradeoffs section enumerates the costs of the chosen approach.",
                "<Callout id=\"tradeoffs\">\nStructured callout.\n</Callout>",
            ),
        ] {
            let response = server
                .dispatch_tool(
                    "proposal_block_patch",
                    serde_json::json!({
                        "id": proposal.id,
                        "selector": { "exact_text": selector_text },
                        "operation": "replace",
                        "block_mdx": block_mdx,
                        "native_skill_name": "visual-spec",
                        "native_skill_version": visual_spec_version,
                    }),
                )
                .await
                .unwrap();
            assert!(
                response.get("error").is_none(),
                "patch failed: {:?}",
                response.get("error")
            );
        }

        // Final body_format must be mdx.
        let stored = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(stored.body_format, "mdx");

        // proposal_export must succeed and return MDX with frontmatter.
        let exported = server
            .dispatch_tool("proposal_export", serde_json::json!({ "id": proposal.id }))
            .await
            .unwrap();
        assert!(
            exported.get("error").is_none(),
            "proposal_export failed after MDX enrichment: {:?}",
            exported.get("error")
        );
        let mdx = exported
            .get("mdx")
            .and_then(|v| v.as_str())
            .expect("proposal_export.mdx must be a non-empty string for mdx proposals");
        assert!(
            mdx.contains("body_format: mdx"),
            "exported MDX frontmatter must record body_format: mdx"
        );
        assert!(
            mdx.matches("---").count() >= 2,
            "exported MDX must include the YAML frontmatter delimiters"
        );

        // The exported MDX body must parse into the same blocks as the
        // stored body.  This is the round-trip fidelity contract.
        let original_blocks =
            parse_mdx_blocks(&stored.body).expect("stored body must parse as MDX");
        let exported_body = mdx
            .splitn(3, "---")
            .nth(2)
            .expect("exported MDX must have a body section after frontmatter")
            .trim_start_matches('\n');
        let exported_blocks =
            parse_mdx_blocks(exported_body).expect("exported body must parse as MDX");
        assert_eq!(
            exported_blocks, original_blocks,
            "exported MDX blocks must match the stored body blocks byte-for-byte"
        );
        let exported_ids: Vec<&str> = exported_blocks.iter().map(|b| b.id.as_str()).collect();
        assert!(exported_ids.contains(&"opening"));
        assert!(exported_ids.contains(&"repo-layout"));
        assert!(exported_ids.contains(&"tradeoffs"));

        // Body validation must succeed end-to-end on the stored body.
        validate_mdx_blocks(&stored.body)
            .expect("enriched body must validate as MDX after the refinement loop");

        // Unrelated sections must survive byte-for-byte — proving no
        // monolithic whole-body rewrite happened.
        for anchor in [
            "# Visual-spec authoring integration",
            "## Open Questions",
            "The open-questions section collects uncertainties for the team.",
        ] {
            assert!(
                stored.body.contains(anchor),
                "unrelated anchor {anchor:?} must be preserved verbatim after the \
                 refinement loop; body was:\n{}",
                stored.body
            );
        }
    }

    /// AC: the planner workflow surfaces remain lazy.  Concretely:
    ///   * the `proposal_address.md` planner prompt does not inline the
    ///     block vocabulary or skill body (verified by re-asserting the
    ///     prompts-tests contract from kepb at the workflow-regression
    ///     level),
    ///   * the catalog pull surface is the single source of block
    ///     vocabulary (verified by ensuring the prompt does not name any
    ///     block tag from `proposal_block_catalog`),
    ///   * the active native-skill version stamped on patch revisions is
    ///     identical to the version returned by `native_skill_version` so the
    ///     registry remains the single source of truth.
    ///
    /// This test ties the lazy-surfaces contract to the actual refinement
    /// loop: any future edit that bakes vocabulary into the prompt, or that
    /// drifts the patch-attribute version away from the registry version,
    /// is caught here.
    #[test]
    fn refinement_loop_workflow_surfaces_remain_lazy() {
        // (a) The planner proposal-address prompt must not inline block
        //     vocabulary.  Re-assert the prompt-test contract at the
        //     workflow-regression level.
        let prompt = include_str!("../../../../djinn-roles/src/prompts/proposal_address.md");
        let catalog = proposal_block_catalog();
        for entry in &catalog {
            assert!(
                !prompt.contains(&entry.tag),
                "proposal_address.md must not inline block tag {:?} from the catalog",
                entry.tag
            );
            assert!(
                !prompt.contains(&entry.block_type),
                "proposal_address.md must not inline block type {:?} from the catalog",
                entry.block_type
            );
        }
        // Generic vocabulary surface must not appear either.
        assert!(
            !prompt.contains("block_types"),
            "proposal_address.md must not reference a `block_types` catalog list"
        );

        // (b) The catalog pull surface and the native-skill version stamp
        //     remain the single source of truth.  The registry version
        //     returned by `native_skill_version` is exactly what planners
        //     stamp on patch revisions — there is no parallel version
        //     constant the prompt or tests could drift against.
        //     `ResolvedSkill` does not carry `version` (that field is reserved
        //     for the immutable native registry), so we only assert that the
        //     planner role resolves `visual-spec` here.
        let registry_version = native_skill_version("visual-spec")
            .expect("native_skill_version must return the active visual-spec version");
        let resolved = resolved_native_skills_for_role("planner");
        assert!(
            resolved.iter().any(|s| s.name == "visual-spec"),
            "planner must resolve visual-spec"
        );
        assert!(
            !registry_version.is_empty(),
            "registry version must be a non-empty stamp"
        );
    }

    /// AC: the full integrated end-to-end refinement loop — markdown draft ->
    /// skill/catalog resolution -> 3 targeted patches -> MDX export — wires
    /// together every y4td surface into a single deterministic regression.
    /// This is the load-bearing test the task acceptance criteria converge on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_loop_end_to_end_ties_all_y4td_surfaces_together() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());

        // (1) Planner authoring session must resolve the native `visual-spec`
        //     skill (y8p2 surface) — verifies the lazy loading contract.
        let resolved_skills = resolved_native_skills_for_role("planner");
        assert!(
            resolved_skills.iter().any(|s| s.name == "visual-spec"),
            "planner must resolve visual-spec via the native registry"
        );
        let registry_version = native_skill_version("visual-spec")
            .expect("native_skill_version must return the active visual-spec version");
        assert!(
            !registry_version.is_empty(),
            "native_skill_version must return a non-empty stamp"
        );

        // (2) The planner pulls the lean catalog on demand (ilqx surface) —
        //     block vocabulary is never inlined into the proposal write
        //     schemas (verified separately by the prompt schema-lean
        //     regression in `schema_lean_tests`).
        let catalog = proposal_block_catalog();
        assert_eq!(
            catalog.len(),
            14,
            "get_block_catalog must expose the full v1 vocabulary on demand"
        );

        // (3) Create a markdown-only proposal draft.
        let proposal = repo
            .create(ProposalCreateInput {
                title: "End-to-end y4td regression",
                body: DRAFT_BODY,
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        assert_eq!(proposal.body_format, "markdown");
        assert_eq!(proposal.latest_revision_seq, 1);

        // (4) Apply 3 sequential targeted block patches through the real
        //     `proposal_block_patch` MCP surface — never a whole-body
        //     `proposal_update`.  Each patch carries the active visual-spec
        //     version from the registry for revision attribution.
        for (selector_text, block_mdx) in [
            (
                "The opening paragraph introduces the proposal and explains its purpose.",
                "<RichText id=\"opening\">\nStructured opening.\n</RichText>",
            ),
            (
                "The approach section describes the high-level plan in prose.",
                "<FileTree id=\"repo-layout\" name=\"repo\" />",
            ),
            (
                "The tradeoffs section enumerates the costs of the chosen approach.",
                "<Callout id=\"tradeoffs\">\nStructured callout.\n</Callout>",
            ),
        ] {
            let response = server
                .dispatch_tool(
                    "proposal_block_patch",
                    serde_json::json!({
                        "id": proposal.id,
                        "selector": { "exact_text": selector_text },
                        "operation": "replace",
                        "block_mdx": block_mdx,
                        "native_skill_name": "visual-spec",
                        "native_skill_version": registry_version,
                    }),
                )
                .await
                .unwrap();
            assert!(
                response.get("error").is_none(),
                "proposal_block_patch failed: {:?}",
                response.get("error")
            );
        }

        // (5) Final state: body_format=mdx, latest_revision_seq=4 (1 seed + 3 patches).
        let final_state = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(final_state.body_format, "mdx");
        assert_eq!(final_state.latest_revision_seq, 4);

        // (6) Revision metadata: every patch revision carries
        //     `targeted_block_patch` + the registry's visual-spec version.
        let shown = server
            .dispatch_tool("proposal_show", serde_json::json!({ "id": proposal.id }))
            .await
            .unwrap();
        let revisions = shown
            .get("revisions")
            .and_then(|v| v.as_array())
            .expect("proposal_show.revisions must be a JSON array");
        assert_eq!(revisions.len(), 4, "1 seed + 3 patches = 4 revisions");
        for (idx, rev) in revisions.iter().enumerate().skip(1) {
            let meta: Value = serde_json::from_str(
                rev.get("event_metadata")
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| panic!("rev #{idx} must expose event_metadata")),
            )
            .unwrap_or_else(|e| panic!("rev #{idx} event_metadata must be JSON: {e}"));
            assert_eq!(meta["change_kind"], "targeted_block_patch");
            assert_eq!(meta["native_skill_name"], "visual-spec");
            assert_eq!(meta["native_skill_version"], registry_version);
        }

        // (7) The final enriched proposal exports as valid MDX through
        //     `proposal_export`, with all 3 patched blocks intact.
        let exported = server
            .dispatch_tool("proposal_export", serde_json::json!({ "id": proposal.id }))
            .await
            .unwrap();
        assert!(exported.get("error").is_none());
        let mdx = exported
            .get("mdx")
            .and_then(|v| v.as_str())
            .expect("export must return mdx for body_format=mdx proposals");
        let exported_body = mdx
            .splitn(3, "---")
            .nth(2)
            .expect("exported MDX must have a body section after frontmatter")
            .trim_start_matches('\n');
        let exported_blocks =
            parse_mdx_blocks(exported_body).expect("exported body must parse as MDX");
        let exported_ids: Vec<&str> = exported_blocks.iter().map(|b| b.id.as_str()).collect();
        assert!(exported_ids.contains(&"opening"));
        assert!(exported_ids.contains(&"repo-layout"));
        assert!(exported_ids.contains(&"tradeoffs"));
    }
}

// ── Router composition ────────────────────────────────────────────────────────

impl DjinnMcpServer {
    /// Composite router for all proposal tools (CRUD/targets + feedback +
    /// signoff + remaining lifecycle tools).
    /// Combines the create/import/export/show/list/target router from `create.rs`,
    /// the feedback router from `feedback.rs`, the signoff router from `signoff.rs`,
    /// and the graduate/stop-build/reconcile router here.
    pub fn proposal_tool_router() -> rmcp::handler::server::router::tool::ToolRouter<Self> {
        Self::proposal_create_tool_router()
            + Self::proposal_feedback_tool_router()
            + Self::proposal_signoff_tool_router()
            + Self::proposal_remaining_tool_router()
    }
}
