use super::*;
use djinn_core::clock::{Clock, SystemClock};
use djinn_core::models::{CiStatus, TaskPrCiSnapshotInput};

impl CoordinatorActor {
    pub(crate) async fn poll_pr_statuses(&mut self) {
        // PR polling runs outside any MCP request scope, so there is no
        // `SESSION_USER_TOKEN` task-local to read. Each task's GitHub client
        // is built from its project's GitHub App installation token
        // (resolved per-task inside the loops below).
        self.poll_pr_draft_tasks().await;
        self.poll_pr_review_tasks().await;
        self.poll_pr_review_stuck_tasks().await;
    }

    // ── pr_draft polling (CI monitoring) ─────────────────────────────────────

    /// Poll tasks in `pr_draft` status: wait for CI to pass, then undraft the PR.
    pub(crate) async fn poll_pr_draft_tasks(&mut self) {
        let task_repo = self.task_repo();
        let project_repo = djinn_db::ProjectRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let pr_draft_tasks = match task_repo.list_by_status("pr_draft").await {
            Ok(tasks) => tasks,
            Err(e) => {
                tracing::warn!(error = %e, "PR poller: failed to query pr_draft tasks");
                return;
            }
        };

        let tasks_with_pr: Vec<_> = pr_draft_tasks
            .into_iter()
            .filter(|t| t.pr_url.is_some())
            .collect();

        if tasks_with_pr.is_empty() {
            return;
        }

        tracing::debug!(
            count = tasks_with_pr.len(),
            "PR poller: checking {} pr_draft task(s)",
            tasks_with_pr.len()
        );

        for task in tasks_with_pr {
            // ── Minimum-age guard ───────────────────────────────────────────
            // Skip tasks that just entered pr_draft — GitHub needs a few
            // seconds to register workflow check-runs for the new commit.
            let first_seen = *self
                .pr_draft_first_seen
                .entry(task.id.clone())
                .or_insert_with(|| SystemClock::new().now_instant());
            let age = first_seen.elapsed();
            if age < Duration::from_secs(PR_DRAFT_MIN_AGE_SECS as u64) {
                tracing::debug!(
                    task_id = %task.short_id,
                    age_secs = age.as_secs(),
                    "PR poller: pr_draft task too young, waiting for check-runs to register"
                );
                continue;
            }

            let Some(pr_url) = task.pr_url.as_deref() else {
                tracing::warn!(
                    task_id = %task.short_id,
                    "PR poller: pr_draft task lost PR URL before checks; skipping"
                );
                continue;
            };
            let Some((owner, repo, pull_number)) = parse_pr_url(pr_url) else {
                tracing::warn!(
                    task_id = %task.short_id,
                    pr_url,
                    "PR poller: unrecognised PR URL format, skipping"
                );
                continue;
            };

            let gh_client = match resolve_installation_client(&project_repo, &task.project_id).await
            {
                Some(c) => c,
                None => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        project_id = %task.project_id,
                        "PR poller: no installation_id on project row; skipping (legacy project?)"
                    );
                    continue;
                }
            };
            let gh_client = &gh_client;

            // Fetch current PR state + CI check runs.
            let (pr, checks) = match gh_client.get_pull_request(&owner, &repo, pull_number).await {
                Ok(result) => result,
                Err(e) => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        error = %e,
                        "PR poller: failed to fetch PR status"
                    );
                    // Record `unknown` for the existing snapshot so downstream
                    // tasks see updated observation timestamps without
                    // escalating to remediation.
                    let pr_number = pull_number as i64;
                    self.record_ci_snapshot_unavailable(&task.id, &task.short_id, pr_number)
                        .await;
                    continue;
                }
            };

            // ── Record CI snapshot (sole writer for GitHub-derived fields) ──
            let pr_number = pull_number as i64;
            let ci_status = self
                .record_ci_snapshot(
                    &task.id,
                    &task.short_id,
                    pr_number,
                    &pr.head.sha,
                    &pr.base.ref_name,
                    pull_number,
                    gh_client,
                    &owner,
                    &repo,
                    &checks,
                )
                .await;

            // ── Tripwire active-hold reconciliation ──────────────────────────
            // After the current PR/head is fetched, check whether an active
            // tripwire hold exists for this head SHA and whether the
            // human-review-hold label is still present. If the label was
            // removed outside the release path, reapply it and log a tamper
            // event. This must run BEFORE any transition that would advance
            // a held PR (merge, undraft, etc.).
            if self
                .reconcile_tripwire_hold(&task, pull_number, &pr.head.sha)
                .await
            {
                // Active hold exists (or was just reapplied) — do not
                // advance the PR this tick. The task is already parked by
                // the gate-held path; reconciliation just ensures the label
                // stays on.
                self.pr_status_cache.remove(&task.id);
                self.pr_draft_first_seen.remove(&task.id);
                self.review_stuck_sha_first_seen.remove(&task.id);
                continue;
            }

            // ── Merged? ───────────────────────────────────────────────────────
            if pr.merged == Some(true) {
                tracing::info!(
                    task_id = %task.short_id,
                    pr = pull_number,
                    "PR poller: PR merged → closing task"
                );
                self.apply_pr_merge(&task.id, pr.merge_commit_sha.as_deref())
                    .await;
                self.pr_status_cache.remove(&task.id);
                self.pr_draft_first_seen.remove(&task.id);
                self.review_stuck_sha_first_seen.remove(&task.id);
                continue;
            }

            // ── PR closed without merge ───────────────────────────────────────
            if pr.state == PrState::Closed {
                tracing::info!(
                    task_id = %task.short_id,
                    pr = pull_number,
                    "PR poller: PR closed without merge → force-closing task"
                );
                self.apply_pr_transition(
                    &task.id,
                    TransitionAction::ForceClose,
                    Some("PR was closed without merging"),
                )
                .await;
                self.pr_status_cache.remove(&task.id);
                self.pr_draft_first_seen.remove(&task.id);
                self.review_stuck_sha_first_seen.remove(&task.id);
                continue;
            }

            // ── CI gate decision (from durable snapshot) ──────────────────────
            // The durable CI snapshot recorded above is the authority for
            // whether the PR may advance.  Pending/Unknown hold in pr_draft
            // (derived display state: awaiting_ci); Failing routes through
            // remediation; Passing proceeds to merge-conflict check + undraft.
            match ci_status {
                CiStatus::Pending | CiStatus::Unknown => {
                    // No-CI-configured fast path: after the min-age guard
                    // elapses, an empty check-run list means the repo has
                    // no CI configured — treat as green.
                    if checks.check_runs.is_empty() {
                        tracing::info!(
                            task_id = %task.short_id,
                            pr = pull_number,
                            "PR poller: no CI check-runs found after min-age guard — treating as passed"
                        );
                        self.persist_ci_snapshot(
                            &task.id,
                            pull_number,
                            &pr.head.sha,
                            CiStatus::Passing,
                            vec![],
                            None,
                            0,
                            None,
                        )
                        .await;
                    } else {
                        // CI checks still running or state unknown — hold
                        // in pr_draft without failure escalation or re-dispatch.
                        continue;
                    }
                }
                CiStatus::Failing => {
                    // Required checks have failed.  Route through the shared
                    // CI-failure handler which filters to blocking (required)
                    // checks, applies the same-signature/scope-inversion/
                    // diff-empty/escalation logic, and either reworks the
                    // task or parks it on a remediation blocker.
                    let failed_checks: Vec<&CheckRun> = checks
                        .check_runs
                        .iter()
                        .filter(|cr| {
                            matches!(
                                cr.conclusion.as_deref(),
                                Some("failure") | Some("timed_out") | Some("cancelled")
                            )
                        })
                        .collect();
                    let handled = self
                        .handle_ci_failure(
                            gh_client,
                            &task,
                            &pr,
                            &failed_checks,
                            pr_url,
                            &owner,
                            &repo,
                            pull_number,
                        )
                        .await;
                    if handled {
                        self.pr_status_cache.remove(&task.id);
                        self.pr_draft_first_seen.remove(&task.id);
                        self.review_stuck_sha_first_seen.remove(&task.id);
                        continue;
                    }
                    // Advisory-only failures slipped through (required checks
                    // are green).  Overwrite the failing snapshot with passing.
                    self.persist_ci_snapshot(
                        &task.id,
                        pull_number,
                        &pr.head.sha,
                        CiStatus::Passing,
                        vec![],
                        None,
                        0,
                        None,
                    )
                    .await;
                }
                CiStatus::Passing => {
                    // All required checks passed — proceed to merge-conflict
                    // check and undraft.
                }
            }

            // CI passed (or no CI configured, or advisory-only failures).
            // Check for merge conflicts before undrafting.
            if pr.mergeable == Some(false) {
                // Clean-merge fast path (offloaded): try to resolve the conflict
                // mechanically (merge target → task branch, push) in a background
                // task before dispatching any agent. `Merged` → the PR refreshes
                // and we skip the reopen; `InFlight` → skip this tick and
                // re-evaluate next tick (the heavy merge must not block the
                // coordinator); `Reopen` → fall through to the agent rework flow.
                match self.poll_auto_merge_fast_path(&task.id, &task.short_id, &task.project_id) {
                    AutoMergeFastPathState::Merged => {
                        self.pr_status_cache.remove(&task.id);
                        self.pr_draft_first_seen.remove(&task.id);
                        self.review_stuck_sha_first_seen.remove(&task.id);
                        continue;
                    }
                    AutoMergeFastPathState::InFlight => {
                        // Background merge running — don't reopen, don't double-fire.
                        continue;
                    }
                    AutoMergeFastPathState::Reopen => {}
                }
                tracing::info!(
                    task_id = %task.short_id,
                    pr = pull_number,
                    "PR poller: draft PR has merge conflicts → reopening task for rework"
                );
                let reason = self
                    .build_pr_conflict_reason(&task.short_id, &task.project_id)
                    .await;
                self.apply_pr_transition(&task.id, TransitionAction::PrConflict, Some(&reason))
                    .await;
                // Reactive auto-blocker: if exactly one racing same-epic sibling
                // is landing on main, make this task WAIT for it (beside the
                // reopen above) instead of looping on the moving main.
                self.add_conflict_blocker_for_sibling(&task).await;
                self.pr_status_cache.remove(&task.id);
                self.pr_draft_first_seen.remove(&task.id);
                self.review_stuck_sha_first_seen.remove(&task.id);
                continue;
            }

            // ── Tripwire gate: deterministic pre-undraft evaluation ──────
            // Evaluate the PR diff against the tripwire policy before the
            // PR can proceed to undraft/auto-merge. This is a pure
            // deterministic gate — no LLM or provider call.
            let tripwire_result = match super::tripwire_gate::evaluate_tripwire_gate(
                gh_client,
                &owner,
                &repo,
                pull_number,
                &task.id,
                &task.project_id,
                &pr.head.sha,
            )
            .await
            {
                Ok(result) => result,
                Err(e) => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        pr = pull_number,
                        error = %e,
                        "PR poller: tripwire gate evaluation failed — proceeding without gate (will retry next tick)"
                    );
                    // Non-fatal: skip this tick so the gate can be
                    // re-evaluated next tick rather than bypassing it.
                    continue;
                }
            };

            // Log the gate activity event (held, passed, or report-only).
            let activity_payload =
                serde_json::to_string(&tripwire_result.payload).unwrap_or_default();
            if let Err(e) = self
                .task_repo()
                .log_activity(
                    Some(&task.id),
                    "coordinator",
                    "system",
                    tripwire_result.event_type,
                    &activity_payload,
                )
                .await
            {
                tracing::warn!(
                    task_id = %task.short_id,
                    pr = pull_number,
                    error = %e,
                    "PR poller: failed to log tripwire gate activity"
                );
            }

            match tripwire_result.decision.outcome {
                crate::tripwires::GateOutcome::Held => {
                    // Enforcement findings exist — apply human-review-hold
                    // idempotently and park the source task so it cannot be
                    // dispatched until the hold is resolved.
                    tracing::info!(
                        task_id = %task.short_id,
                        pr = pull_number,
                        head_sha = %pr.head.sha,
                        enforcement_count = tripwire_result.decision.enforcement_finding_count,
                        "PR poller: tripwire gate HELD — creating human-review hold"
                    );

                    let findings_summary = tripwire_result
                        .decision
                        .findings
                        .iter()
                        .map(|f| {
                            format!(
                                "- `{}` ({}) — {}",
                                f.rule_id.as_str(),
                                f.reason_code,
                                f.evidence.path,
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");

                    let hold_reason = format!(
                        "Tripwire gate held for PR head `{}`.\n\n\
                         Policy revision: `{}`\n\
                         Enforcement findings ({}):\n{}",
                        &pr.head.sha[..12.min(pr.head.sha.len())],
                        tripwire_result.decision.policy_revision,
                        tripwire_result.decision.enforcement_finding_count,
                        findings_summary,
                    );

                    // Create a human-review remediation task (idempotent —
                    // skips if the source is already held by an unresolved
                    // blocker).
                    self.create_remediation_task(
                        &task.id,
                        &hold_reason,
                        &task.project_id,
                        crate::dispatch::RemediationKind::HumanReview,
                    )
                    .await;

                    // Park the source so it stops being re-dispatched.
                    self.park_source_open(&task.id, &hold_reason).await;
                    self.pr_status_cache.remove(&task.id);
                    self.pr_draft_first_seen.remove(&task.id);
                    self.review_stuck_sha_first_seen.remove(&task.id);
                    continue;
                }
                crate::tripwires::GateOutcome::Passed
                | crate::tripwires::GateOutcome::ReportOnly => {
                    // Gate passed (with optional advisory findings) —
                    // proceed to undraft.
                    if tripwire_result.decision.outcome == crate::tripwires::GateOutcome::ReportOnly
                    {
                        tracing::info!(
                            task_id = %task.short_id,
                            pr = pull_number,
                            head_sha = %pr.head.sha,
                            report_only_count = tripwire_result.decision.report_only_finding_count,
                            "PR poller: tripwire gate PASSED with report-only findings"
                        );
                    } else {
                        tracing::info!(
                            task_id = %task.short_id,
                            pr = pull_number,
                            head_sha = %pr.head.sha,
                            "PR poller: tripwire gate PASSED"
                        );
                    }
                }
            }

            // All CI passed, no merge conflicts, tripwire gate passed —
            // undraft the PR, then transition.
            tracing::info!(
                task_id = %task.short_id,
                pr = pull_number,
                "PR poller: CI passed on draft PR → undrafting and marking ready for review"
            );
            match gh_client.mark_pr_ready_for_review(&pr.node_id).await {
                Ok(_) => {
                    // Best-effort: enable auto-merge so GitHub handles
                    // approval-and-checks gating + merge-queue enqueue.
                    // If it fails (repo doesn't allow auto-merge, branch
                    // protection refuses, etc.) the pr_review loop falls
                    // back to the legacy REST merge path.
                    enable_auto_merge_best_effort(
                        gh_client,
                        &task.short_id,
                        pull_number,
                        &pr.node_id,
                        &pr.title,
                    )
                    .await;

                    self.apply_pr_transition(&task.id, TransitionAction::PrUndraft, None)
                        .await;
                    self.pr_status_cache.remove(&task.id);
                    self.pr_draft_first_seen.remove(&task.id);
                    self.review_stuck_sha_first_seen.remove(&task.id);
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        pr = pull_number,
                        error = %e,
                        "PR poller: failed to undraft PR (will retry next tick)"
                    );
                    // Don't transition — will retry next tick.
                }
            }
        }
    }

    // ── Tripwire active-hold reconciliation ──────────────────────────────

    /// Reconcile the tripwire active-hold state for a task's current PR head.
    ///
    /// This runs after the current PR/head is fetched and before any
    /// transition that would advance a held PR. It:
    ///
    /// 1. Queries the task's activity log for `tripwire.gate.held` and
    ///    `tripwire.hold.released` events.
    /// 2. Computes the active-hold state for the current head SHA (older
    ///    heads are superseded).
    /// 3. If an active hold exists but the task no longer carries
    ///    `human-review-hold`, reapplies the label idempotently and logs a
    ///    `tripwire.tamper.label_removed` event.
    ///
    /// Returns `true` when an active hold exists for the current head (the
    /// PR must NOT advance). Returns `false` when there is no active hold
    /// (the PR may proceed through the normal flow).
    ///
    /// **Fail-closed:** if the activity-log query fails, this returns `true`
    /// (hold assumed) rather than letting the PR advance — an inability to
    /// verify hold state must not bypass the gate.
    pub(crate) async fn reconcile_tripwire_hold(
        &self,
        task: &Task,
        pr_number: u64,
        head_sha: &str,
    ) -> bool {
        let task_repo = self.task_repo();

        // Query activity entries for this task.
        let entries = match task_repo.list_activity(&task.id).await {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!(
                    task_id = %task.short_id,
                    error = %e,
                    "PR poller: failed to query activity for tripwire hold reconciliation — failing closed (hold assumed)"
                );
                return true;
            }
        };

        // Map to lightweight references for the pure computation.
        let entry_refs: Vec<crate::tripwires::ActivityEntryRef> = entries
            .iter()
            .map(crate::tripwires::ActivityEntryRef::from_entry)
            .collect();

        // Compute active-hold state for the current head SHA.
        let state = crate::tripwires::compute_active_hold_state(&entry_refs, head_sha);

        if !state.held {
            return false;
        }

        // Active hold exists — check whether the label is still present.
        let has_label = crate::roles::is_human_review_hold(task);

        if !has_label {
            // Label was removed outside the release path — reapply it and
            // log a tamper event. Both operations must succeed; if either
            // fails we fail closed (return true) so the PR does not advance.
            let now = ::time::OffsetDateTime::now_utc()
                .format(&::time::format_description::well_known::Rfc3339)
                .unwrap_or_default();

            let recon = crate::tripwires::check_label_tamper(
                &state,
                &task.id,
                &task.project_id,
                Some(pr_number),
                has_label,
                "pr_poller",
                &now,
            );

            if recon.tamper_detected {
                // Reapply the label idempotently.
                let labels_json = add_hold_label_to_existing(&task.labels);
                if let Err(e) = task_repo.update_labels(&task.id, &labels_json).await {
                    tracing::error!(
                        task_id = %task.short_id,
                        pr = pr_number,
                        head_sha = %head_sha,
                        error = %e,
                        "PR poller: FAILED to reapply human-review-hold label during tamper reconciliation — failing closed"
                    );
                    return true;
                }

                // Log the tamper event.
                if let Some(ref payload) = recon.payload {
                    let payload_json = serde_json::to_string(payload).unwrap_or_default();
                    if let Err(e) = task_repo
                        .log_activity(
                            Some(&task.id),
                            "coordinator",
                            "system",
                            crate::tripwires::TRIPWIRE_EVENT_TAMPER_LABEL_REMOVED,
                            &payload_json,
                        )
                        .await
                    {
                        tracing::error!(
                            task_id = %task.short_id,
                            pr = pr_number,
                            head_sha = %head_sha,
                            error = %e,
                            "PR poller: FAILED to log tripwire.tamper.label_removed event — failing closed"
                        );
                        return true;
                    }
                }

                tracing::info!(
                    task_id = %task.short_id,
                    pr = pr_number,
                    head_sha = %head_sha,
                    active_finding_count = state.active_findings.len(),
                    "PR poller: tripwire tamper detected — human-review-hold label reapplied and tamper event logged"
                );
            }
        }

        // Active hold exists — PR must not advance.
        true
    }

    // ── needs_task_review polling (review-stuck CI monitoring) ───────────────

    /// Poll tasks parked in `needs_task_review`: if blocking CI is terminal red
    /// and the PR head SHA has not advanced for the review-stuck window, route a
    /// Planner intervention for the reviewer loop with the CI failure details.
    pub(crate) async fn poll_pr_review_stuck_tasks(&mut self) {
        let task_repo = self.task_repo();
        let project_repo = djinn_db::ProjectRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let review_tasks = match task_repo.list_by_status("needs_task_review").await {
            Ok(tasks) => tasks,
            Err(e) => {
                tracing::warn!(error = %e, "PR poller: failed to query needs_task_review tasks");
                return;
            }
        };

        let tasks_with_pr: Vec<_> = review_tasks
            .into_iter()
            .filter(|t| t.pr_url.is_some())
            .collect();

        if tasks_with_pr.is_empty() {
            return;
        }

        tracing::debug!(
            count = tasks_with_pr.len(),
            "PR poller: checking {} needs_task_review task(s) for review-stuck CI",
            tasks_with_pr.len()
        );

        for task in tasks_with_pr {
            let Some(pr_url) = task.pr_url.as_deref() else {
                tracing::warn!(
                    task_id = %task.short_id,
                    "PR poller: needs_task_review task lost PR URL before review-stuck check; skipping"
                );
                self.review_stuck_sha_first_seen.remove(&task.id);
                continue;
            };
            let Some((owner, repo, pull_number)) = parse_pr_url(pr_url) else {
                tracing::warn!(
                    task_id = %task.short_id,
                    pr_url,
                    "PR poller: unrecognised PR URL format for review-stuck check, skipping"
                );
                self.review_stuck_sha_first_seen.remove(&task.id);
                continue;
            };

            let gh_client = match resolve_installation_client(&project_repo, &task.project_id).await
            {
                Some(c) => c,
                None => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        project_id = %task.project_id,
                        "PR poller: no installation_id on project row; skipping review-stuck check"
                    );
                    continue;
                }
            };
            let gh_client = &gh_client;

            let (pr, checks) = match gh_client.get_pull_request(&owner, &repo, pull_number).await {
                Ok(result) => result,
                Err(e) => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        error = %e,
                        "PR poller: failed to fetch PR status for review-stuck check"
                    );
                    // Record `unknown` for existing snapshot.
                    let pr_number = pull_number as i64;
                    self.record_ci_snapshot_unavailable(&task.id, &task.short_id, pr_number)
                        .await;
                    continue;
                }
            };

            // ── Record CI snapshot (sole writer for GitHub-derived fields) ──
            let pr_number = pull_number as i64;
            self.record_ci_snapshot(
                &task.id,
                &task.short_id,
                pr_number,
                &pr.head.sha,
                &pr.base.ref_name,
                pull_number,
                gh_client,
                &owner,
                &repo,
                &checks,
            )
            .await;

            if pr.merged == Some(true) || pr.state == PrState::Closed {
                self.review_stuck_sha_first_seen.remove(&task.id);
                continue;
            }

            let current_sha = pr.head.sha.clone();
            if checks.check_runs.is_empty()
                || !checks.check_runs.iter().all(|cr| cr.status == "completed")
            {
                self.review_stuck_sha_first_seen.remove(&task.id);
                continue;
            }

            let terminal_red: Vec<&CheckRun> = checks
                .check_runs
                .iter()
                .filter(|cr| is_failing_conclusion(cr.conclusion.as_deref()))
                .collect();
            if terminal_red.is_empty() {
                self.review_stuck_sha_first_seen.remove(&task.id);
                continue;
            }

            let required_contexts = self
                .resolve_required_contexts(gh_client, &owner, &repo, &pr.base.ref_name, pull_number)
                .await;
            let blocking = blocking_failed_checks(&terminal_red, required_contexts.as_deref());
            if blocking.is_empty() {
                self.review_stuck_sha_first_seen.remove(&task.id);
                continue;
            }

            // Persist the failing CI snapshot for the review-stuck observation.
            let blocking_names: Vec<String> = blocking.iter().map(|cr| cr.name.clone()).collect();
            self.persist_ci_snapshot(
                &task.id,
                pull_number,
                &current_sha,
                CiStatus::Failing,
                blocking_names,
                None,
                0,
                None,
            )
            .await;

            let first_seen = match self.review_stuck_sha_first_seen.get(&task.id) {
                Some((seen_sha, first_seen)) if seen_sha == &current_sha => *first_seen,
                _ => {
                    let now = SystemClock::new().now_instant();
                    self.review_stuck_sha_first_seen
                        .insert(task.id.clone(), (current_sha.clone(), now));
                    now
                }
            };

            let elapsed = first_seen.elapsed();
            if elapsed < Duration::from_secs((REVIEW_STUCK_WINDOW_MINUTES * 60) as u64) {
                tracing::debug!(
                    task_id = %task.short_id,
                    sha = %current_sha,
                    elapsed_secs = elapsed.as_secs(),
                    "PR poller: needs_task_review task has red CI but review-stuck window has not elapsed"
                );
                continue;
            }

            let failing_check_names: Vec<String> =
                blocking.iter().map(|cr| cr.name.clone()).collect();
            let sections_text = self
                .build_review_stuck_ci_failure_sections(gh_client, &owner, &repo, &blocking)
                .await;
            let reason = format!(
                "Task is stuck in needs_task_review with terminal red blocking CI for at least \
                 {REVIEW_STUCK_WINDOW_MINUTES} minutes on unchanged head SHA `{}`. PR: {}. \
                 Failing blocking check(s): {}.",
                &current_sha[..current_sha.len().min(12)],
                pr_url,
                failing_check_names.join(", ")
            );

            self.review_stuck_sha_first_seen.remove(&task.id);
            let handled = self
                .route_planner_intervention(
                    &task,
                    "reviewer",
                    &reason,
                    Some(&sections_text),
                    task.reopen_count,
                )
                .await;
            if handled {
                tracing::warn!(
                    task_id = %task.short_id,
                    pr = pull_number,
                    sha = %current_sha,
                    failing_checks = ?failing_check_names,
                    "PR poller: review-stuck trigger routed task to Planner intervention"
                );
            }
        }
    }

    async fn build_review_stuck_ci_failure_sections(
        &self,
        gh_client: &GitHubApiClient,
        owner: &str,
        repo: &str,
        blocking: &[&CheckRun],
    ) -> String {
        let mut run_ids: Vec<u64> = Vec::new();
        for cr in blocking {
            if let Some(rid) = parse_actions_run_id(&cr.html_url)
                && !run_ids.contains(&rid)
            {
                run_ids.push(rid);
            }
        }

        let capped = run_ids.len() > MAX_AGGREGATED_CI_RUNS;
        if capped {
            run_ids.truncate(MAX_AGGREGATED_CI_RUNS);
        }

        let mut all_jobs: Vec<ActionsJob> = Vec::new();
        let mut seen_job_ids: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let mut any_fetched = false;
        for rid in &run_ids {
            if let Ok(jobs) = gh_client.list_run_jobs(owner, repo, *rid).await {
                any_fetched = true;
                for job in jobs {
                    if seen_job_ids.insert(job.id) {
                        all_jobs.push(job);
                    }
                }
            }
        }

        let jobs: Option<&[ActionsJob]> = if any_fetched { Some(&all_jobs) } else { None };
        let (mut sections, _) = build_ci_failure_sections(jobs, blocking);
        if capped {
            sections.push(format!(
                "\n_Note: more than {MAX_AGGREGATED_CI_RUNS} workflow runs failed; \
                 showing the first {MAX_AGGREGATED_CI_RUNS}._"
            ));
        }
        sections.join("\n")
    }

    /// Persist a CI gate snapshot for the current PR head observation.
    ///
    /// Write-through only — no lifecycle policy.  Failures are logged as
    /// warnings (consistent with existing pr_poller error patterns) and do
    /// **not** block the caller's control flow.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn persist_ci_snapshot(
        &self,
        task_id: &str,
        pr_number: u64,
        head_sha: &str,
        ci_status: CiStatus,
        blocking_required_check_names: Vec<String>,
        failure_fingerprint: Option<String>,
        same_signature_count: i64,
        last_remediation_base_sha: Option<String>,
    ) {
        let task_repo = self.task_repo();
        let input = TaskPrCiSnapshotInput {
            task_id: task_id.to_owned(),
            pr_number: pr_number as i64,
            head_sha: head_sha.to_owned(),
            ci_status,
            blocking_required_check_names,
            failure_fingerprint,
            same_signature_count,
            last_remediation_base_sha,
        };
        if let Err(e) = task_repo.upsert_ci_snapshot(input).await {
            tracing::warn!(
                task_id = %task_id,
                pr = pr_number,
                ci_status = %ci_status,
                error = %e,
                "PR poller: failed to persist CI snapshot (non-fatal)"
            );
        }
    }

    // ── CI snapshot recording (sole writer for GitHub-derived CI fields) ─────

    /// Record the CI snapshot for a task/PR based on observed GitHub data.
    ///
    /// Called by `pr_poller` — the **sole writer** of GitHub-derived CI snapshot
    /// fields. Determines `ci_status` from required-check state and computes
    /// failure fingerprints using the existing `ci_helpers` utilities.
    ///
    /// When the stored head SHA differs from the observed SHA, the snapshot is
    /// reset to `pending` with stale failure/fingerprint/same-signature data
    /// cleared (delegated to the repository's `reset_ci_snapshot_for_head`).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn record_ci_snapshot(
        &self,
        task_id: &str,
        task_short_id: &str,
        pr_number: i64,
        head_sha: &str,
        base_ref: &str,
        pull_number: u64,
        gh_client: &GitHubApiClient,
        owner: &str,
        repo: &str,
        checks: &djinn_provider::github_api::CheckRunsResponse,
    ) -> CiStatus {
        let task_repo = self.task_repo();

        // ── Head-sha change detection ───────────────────────────────────────
        // When the PR's head SHA changes (new push), reset to `pending` and
        // clear stale failure/fingerprint/same-signature data.
        let stored_sha = task_repo
            .get_ci_snapshot_for_task_pr(task_id, pr_number)
            .await
            .ok()
            .flatten()
            .map(|s| s.head_sha);

        if stored_sha.as_deref() != Some(head_sha) {
            match task_repo
                .reset_ci_snapshot_for_head(task_id, pr_number, head_sha)
                .await
            {
                Ok(_snapshot) => {
                    tracing::info!(
                        task_id = %task_short_id,
                        pr = pull_number,
                        old_sha = ?stored_sha.as_deref().map(|s| &s[..s.len().min(12)]),
                        new_sha = &head_sha[..head_sha.len().min(12)],
                        "PR poller: head SHA changed — reset CI snapshot to pending, cleared stale data"
                    );
                    // If there are no completed checks, the snapshot is now
                    // `pending` for the new head — nothing more to classify.
                    if checks.check_runs.is_empty()
                        || !checks.check_runs.iter().all(|cr| cr.status == "completed")
                    {
                        // Leave as `pending`; the next poll will re-evaluate
                        // when all checks complete.
                        return CiStatus::Pending;
                    }
                    // Fall through to classify the completed checks below.
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = %task_short_id,
                        pr = pull_number,
                        error = %e,
                        "PR poller: failed to reset CI snapshot for head change"
                    );
                    return CiStatus::Unknown;
                }
            }
        }

        // ── Classify check state ────────────────────────────────────────────
        let ci_status = if checks.check_runs.is_empty() {
            // No checks exist — repo has no CI configured. After the
            // minimum-age guard elapses, the poller treats this as green
            // (see the no-CI fast path in `poll_pr_draft_tasks`). The
            // snapshot records `unknown` to distinguish from "checks
            // still running" (`pending`).
            CiStatus::Unknown
        } else if checks.check_runs.iter().all(|cr| cr.status == "completed") {
            let failed_checks: Vec<&CheckRun> = checks
                .check_runs
                .iter()
                .filter(|cr| {
                    matches!(
                        cr.conclusion.as_deref(),
                        Some("failure") | Some("timed_out") | Some("cancelled")
                    )
                })
                .collect();

            let required_contexts = self
                .resolve_required_contexts(gh_client, owner, repo, base_ref, pull_number)
                .await;
            let blocking = blocking_failed_checks(&failed_checks, required_contexts.as_deref());

            if blocking.is_empty() {
                // All required checks passed (or no CI configured).
                CiStatus::Passing
            } else {
                // At least one required check failed.
                CiStatus::Failing
            }
        } else {
            // Checks still running.
            CiStatus::Pending
        };

        // ── Compute fingerprint for failing state ───────────────────────────
        let (failure_fingerprint, blocking_names) = if ci_status == CiStatus::Failing {
            let failed_checks: Vec<&CheckRun> = checks
                .check_runs
                .iter()
                .filter(|cr| {
                    matches!(
                        cr.conclusion.as_deref(),
                        Some("failure") | Some("timed_out") | Some("cancelled")
                    )
                })
                .collect();

            let required_contexts = self
                .resolve_required_contexts(gh_client, owner, repo, base_ref, pull_number)
                .await;
            let blocking = blocking_failed_checks(&failed_checks, required_contexts.as_deref());

            // Build failure sections for fingerprint computation.
            let (sections, _) = ci_helpers::build_ci_failure_sections(None, &blocking);
            let fp = compute_ci_failure_fingerprint(&blocking, &sections);
            let names: Vec<String> = blocking.iter().map(|cr| cr.name.clone()).collect();
            (Some(fp), names)
        } else {
            (None, Vec::new())
        };

        let input = TaskPrCiSnapshotInput {
            task_id: task_id.to_string(),
            pr_number,
            head_sha: head_sha.to_string(),
            ci_status,
            blocking_required_check_names: blocking_names,
            failure_fingerprint,
            // Let the repository layer manage same-signature counting and
            // timestamps via upsert semantics.
            same_signature_count: 0,
            last_remediation_base_sha: None,
        };

        match task_repo.upsert_ci_snapshot(input).await {
            Ok(snapshot) => {
                tracing::debug!(
                    task_id = %task_short_id,
                    pr = pull_number,
                    ci_status = %snapshot.ci_status,
                    blocking_count = snapshot.blocking_required_check_names.len(),
                    fingerprint = ?snapshot.failure_fingerprint,
                    same_sig_count = snapshot.same_signature_count,
                    "PR poller: CI snapshot recorded"
                );
            }
            Err(e) => {
                tracing::warn!(
                    task_id = %task_short_id,
                    pr = pull_number,
                    error = %e,
                    "PR poller: failed to record CI snapshot"
                );
            }
        }

        ci_status
    }

    /// Record `unknown` CI snapshot when GitHub PR/check data is unavailable.
    ///
    /// Called when `gh_client.get_pull_request()` fails. If an existing snapshot
    /// exists with a known head SHA, updates it to `unknown` so downstream tasks
    /// see updated observation timestamps without escalating to remediation.
    pub(crate) async fn record_ci_snapshot_unavailable(
        &self,
        task_id: &str,
        task_short_id: &str,
        pr_number: i64,
    ) {
        let task_repo = self.task_repo();
        if let Ok(Some(existing)) = task_repo
            .get_ci_snapshot_for_task_pr(task_id, pr_number)
            .await
            && !existing.head_sha.is_empty()
            && existing.ci_status != CiStatus::Unknown
        {
            let input = TaskPrCiSnapshotInput {
                task_id: task_id.to_string(),
                pr_number,
                head_sha: existing.head_sha.clone(),
                ci_status: CiStatus::Unknown,
                blocking_required_check_names: Vec::new(),
                failure_fingerprint: None,
                same_signature_count: 0,
                last_remediation_base_sha: None,
            };
            match task_repo.upsert_ci_snapshot(input).await {
                Ok(snapshot) => {
                    tracing::info!(
                        task_id = %task_short_id,
                        pr = pr_number,
                        sha = &snapshot.head_sha[..snapshot.head_sha.len().min(12)],
                        "PR poller: GitHub data unavailable — recorded unknown CI status with updated timestamp"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = %task_short_id,
                        pr = pr_number,
                        error = %e,
                        "PR poller: failed to record unavailable CI snapshot"
                    );
                }
            }
        }
    }

    // ── pr_review polling (review monitoring) ────────────────────────────────
}

/// Decision for a `pr_draft` task based on the durable CI snapshot status.
///
/// Encapsulates the transition matrix so the decision logic is testable in
/// isolation from the async poller machinery.
#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PrDraftCiAction {
    /// CI is passing (or no CI configured after the min-age guard) — proceed
    /// to the merge-conflict check and undraft.
    ///
    /// `needs_passing_persist` is `true` when the caller should persist a
    /// `Passing` snapshot before proceeding (e.g. the no-CI-configured fast
    /// path or the advisory-only-failure fallthrough where the snapshot was
    /// written as `Unknown`/`Failing` by `record_ci_snapshot` but the
    /// lifecycle treats the PR as green).
    Proceed { needs_passing_persist: bool },
    /// CI is pending or unknown — hold in `pr_draft` without failure
    /// escalation, force-close, or worker re-dispatch.
    Hold,
    /// Required CI checks have failed — route through the shared
    /// `handle_ci_failure` remediation/intervention handler.
    RouteToRemediation,
}

/// Decide the `pr_draft` CI gate action from the durable snapshot status and
/// the raw check-run data.
///
/// This is the pure decision function tested in isolation; the poller calls
/// it after `record_ci_snapshot()` to determine the next action.
#[cfg(test)]
pub(crate) fn decide_pr_draft_ci_action(
    ci_status: CiStatus,
    has_check_runs: bool,
) -> PrDraftCiAction {
    match ci_status {
        CiStatus::Pending | CiStatus::Unknown => {
            if !has_check_runs {
                // No CI configured — treat as green after min-age guard.
                PrDraftCiAction::Proceed {
                    needs_passing_persist: true,
                }
            } else {
                PrDraftCiAction::Hold
            }
        }
        CiStatus::Failing => PrDraftCiAction::RouteToRemediation,
        CiStatus::Passing => PrDraftCiAction::Proceed {
            needs_passing_persist: false,
        },
    }
}

/// Add the `human-review-hold` label to an existing labels JSON array,
/// returning the updated JSON string. Idempotent: if the label is already
/// present, the input is returned unchanged.
///
/// `existing_labels` is a JSON-array string (e.g. `["bug","blocked"]`).
/// Malformed JSON is treated as an empty label set.
fn add_hold_label_to_existing(existing_labels: &str) -> String {
    let mut labels: Vec<String> = serde_json::from_str(existing_labels).unwrap_or_default();
    let hold_label = crate::roles::HUMAN_REVIEW_HOLD_LABEL;
    if !labels.iter().any(|l| l == hold_label) {
        labels.push(hold_label.to_owned());
    }
    serde_json::to_string(&labels).unwrap_or_else(|_| format!("[\"{hold_label}\"]"))
}
