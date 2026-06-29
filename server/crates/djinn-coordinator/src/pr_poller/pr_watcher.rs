use super::*;

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
                .or_insert_with(StdInstant::now);
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
                    continue;
                }
            };

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

            // ── CI checks ─────────────────────────────────────────────────────
            if checks.check_runs.is_empty() {
                // No checks exist — repo has no CI configured.  Since the
                // minimum-age guard above already elapsed, treat as green.
                tracing::info!(
                    task_id = %task.short_id,
                    pr = pull_number,
                    "PR poller: no CI check-runs found after min-age guard — treating as passed"
                );
            } else {
                let all_completed = checks.check_runs.iter().all(|cr| cr.status == "completed");
                if !all_completed {
                    // CI checks still running — skip, check next tick.
                    continue;
                }
                // All completed — check for failures.
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
                if !failed_checks.is_empty() {
                    // Route through the shared CI-failure handler: it filters
                    // to *blocking* (required) checks, short-circuits the
                    // diff-empty re-emit, and caps the rework loop with
                    // escalation. Returns `true` when it consumed the event
                    // (reworked or force-closed); `false` when every failure
                    // was non-blocking and we should fall through as if CI
                    // passed.
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
                    // else: only advisory checks failed — fall through to the
                    // undraft path below as if CI is green.
                }
            }

            // CI passed (or no CI configured). Check for merge conflicts before undrafting.
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

            // All CI passed and no merge conflicts — undraft the PR, then transition.
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
                    continue;
                }
            };

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

            let first_seen = match self.review_stuck_sha_first_seen.get(&task.id) {
                Some((seen_sha, first_seen)) if seen_sha == &current_sha => *first_seen,
                _ => {
                    let now = StdInstant::now();
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
                .route_planner_intervention(&task, "reviewer", &reason, Some(&sections_text))
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

    // ── pr_review polling (review monitoring) ────────────────────────────────
}
