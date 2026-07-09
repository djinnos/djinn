use super::*;
use djinn_core::models::CiStatus;

impl CoordinatorActor {
    // Poll `pr_review` tasks: wait for approval or changes, then merge.
    pub(crate) async fn poll_pr_review_tasks(&mut self) {
        let task_repo = self.task_repo();
        let project_repo = djinn_db::ProjectRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let pr_review_tasks = match task_repo.list_by_status("pr_review").await {
            Ok(tasks) => tasks,
            Err(e) => {
                tracing::warn!(error = %e, "PR poller: failed to query pr_review tasks");
                return;
            }
        };

        let tasks_with_pr: Vec<_> = pr_review_tasks
            .into_iter()
            .filter(|t| t.pr_url.is_some())
            .collect();

        if tasks_with_pr.is_empty() {
            return;
        }

        tracing::debug!(
            count = tasks_with_pr.len(),
            "PR poller: checking {} pr_review task(s)",
            tasks_with_pr.len()
        );

        for task in tasks_with_pr {
            let Some(pr_url) = task.pr_url.as_deref() else {
                tracing::warn!(
                    task_id = %task.short_id,
                    "PR poller: pr_review task lost PR URL before checks; skipping"
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
                    // Record `unknown` for existing snapshot.
                    let pr_number = pull_number as i64;
                    self.record_ci_snapshot_unavailable(&task.id, &task.short_id, pr_number)
                        .await;
                    continue;
                }
            };

            // ── Record CI snapshot (sole writer for GitHub-derived fields) ──
            let pr_number_i64 = pull_number as i64;
            self.record_ci_snapshot(
                &task.id,
                &task.short_id,
                pr_number_i64,
                &pr.head.sha,
                &pr.base.ref_name,
                pull_number,
                gh_client,
                &owner,
                &repo,
                &checks,
            )
            .await;

            let current_sha = pr.head.sha.clone();

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
                self.review_stuck_sha_first_seen.remove(&task.id);
                self.merge_fail_count.remove(&task.id);
                self.delegated_to_github.remove(&task.id);
                self.conversations_resolved.remove(&task.id);
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
                self.review_stuck_sha_first_seen.remove(&task.id);
                self.merge_fail_count.remove(&task.id);
                self.delegated_to_github.remove(&task.id);
                self.conversations_resolved.remove(&task.id);
                continue;
            }

            // ── Review state ──────────────────────────────────────────────────
            let reviews = match gh_client
                .list_pr_review_states(&owner, &repo, pull_number)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        error = %e,
                        "PR poller: failed to fetch PR reviews, will retry next tick"
                    );
                    continue;
                }
            };

            // Effective gating decision: latest standing review per reviewer,
            // deduping stale CHANGES_REQUESTED that a reviewer later dismissed
            // or turned into APPROVED (task 2sq6 fix).
            let (changes_requested, has_approved) = effective_review_decision(&reviews);

            if changes_requested {
                tracing::info!(
                    task_id = %task.short_id,
                    pr = pull_number,
                    "PR poller: reviewer requested changes → dispatching review feedback loop"
                );

                // Fetch aggregated feedback (reviews + inline comments).
                let feedback = match gh_client
                    .fetch_pr_review_feedback(&owner, &repo, pull_number, pr_url)
                    .await
                {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::warn!(
                            task_id = %task.short_id,
                            error = %e,
                            "PR poller: failed to fetch PR review feedback, using empty"
                        );
                        PrReviewFeedback {
                            pull_number,
                            pr_url: pr_url.to_owned(),
                            change_request_reviews: Vec::new(),
                            inline_comments: Vec::new(),
                        }
                    }
                };

                // Attach feedback + handle escalation threshold.
                self.attach_pr_review_feedback(&task.id, &task.short_id, feedback)
                    .await;

                // Evaluate CI in the same tick so changes-requested AND
                // failing required checks surface together. Use
                // `log_ci_failure_comment` directly (not `handle_ci_failure`)
                // to avoid double-transitioning.
                let blocking_failed: Vec<&CheckRun> = checks
                    .check_runs
                    .iter()
                    .filter(|cr| {
                        matches!(
                            cr.conclusion.as_deref(),
                            Some("failure") | Some("timed_out") | Some("cancelled")
                        )
                    })
                    .collect();
                if !blocking_failed.is_empty() {
                    let required_contexts: Option<Vec<String>> = self
                        .resolve_required_contexts(
                            gh_client,
                            &owner,
                            &repo,
                            &pr.base.ref_name,
                            pull_number,
                        )
                        .await;
                    let blocking =
                        blocking_failed_checks(&blocking_failed, required_contexts.as_deref());
                    let blocking_names: std::collections::HashSet<&str> =
                        blocking.iter().map(|cr| cr.name.as_str()).collect();
                    let advisory: Vec<&CheckRun> = blocking_failed
                        .iter()
                        .filter(|cr| !blocking_names.contains(cr.name.as_str()))
                        .copied()
                        .collect();
                    if !blocking.is_empty() {
                        // Persist the failing CI snapshot for the
                        // changes-requested + blocking-CI path.  No
                        // fingerprint or same-signature tracking here —
                        // this path intentionally avoids the
                        // handle_ci_failure cycle-cap/diff-empty logic
                        // (the reviewer's request is the primary driver).
                        let snap_blocking_names: Vec<String> =
                            blocking.iter().map(|cr| cr.name.clone()).collect();
                        self.persist_ci_snapshot(
                            &task.id,
                            pull_number,
                            &current_sha,
                            CiStatus::Failing,
                            snap_blocking_names,
                            None,
                            0,
                            None,
                        )
                        .await;
                        tracing::info!(
                            task_id = %task.short_id,
                            pr = pull_number,
                            blocking_count = blocking.len(),
                            "PR poller: changes requested AND required CI failing in same tick → logging both feedbacks before single reopen"
                        );
                    }
                    // Even when every failure is non-required, the reviewer-
                    // driven reopen below still spawns a worker — give it the
                    // advisory list as context rather than nothing.
                    if !blocking.is_empty() || !advisory.is_empty() {
                        self.log_ci_failure_comment(
                            &task.id,
                            &blocking,
                            &advisory,
                            pr_url,
                            &current_sha,
                            gh_client,
                            &owner,
                            &repo,
                        )
                        .await;
                    }
                }

                // Record the rejected submission fingerprint at the task
                // level so the live submit-work guard can detect no-progress
                // resubmissions across task runs after a PR reviewer requests
                // changes.
                self.record_pr_rejection_fingerprint(&task.id).await;

                self.apply_pr_transition(
                    &task.id,
                    TransitionAction::PrChangesRequested,
                    Some("Reviewer requested changes on PR"),
                )
                .await;
                self.pr_status_cache.remove(&task.id);
                self.review_stuck_sha_first_seen.remove(&task.id);
                self.merge_fail_count.remove(&task.id);
                self.delegated_to_github.remove(&task.id);
                self.conversations_resolved.remove(&task.id);
                continue;
            }

            // ── CI checks on review PR (cached per head SHA) ──────────────────
            // Only skip CI re-check if the SHA hasn't changed AND we previously
            // confirmed all checks completed successfully.  If checks were still
            // in-progress last time we looked, we must re-check.
            let sha_changed = self
                .pr_status_cache
                .get(&task.id)
                .map(|cached| cached != &current_sha)
                .unwrap_or(true);

            if (sha_changed || !self.pr_status_cache.contains_key(&task.id))
                && !checks.check_runs.is_empty()
            {
                let all_completed = checks.check_runs.iter().all(|cr| cr.status == "completed");

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
                    // Shared CI-failure handler (blocking-only filter +
                    // diff-empty short-circuit + capped, escalating rework).
                    // Returns `true` when it consumed the event; `false` when
                    // only advisory/preview checks failed, in which case we
                    // fall through as if CI is green.
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
                        self.review_stuck_sha_first_seen.remove(&task.id);
                        self.merge_fail_count.remove(&task.id);
                        self.delegated_to_github.remove(&task.id);
                        self.conversations_resolved.remove(&task.id);
                        continue;
                    }
                    // else: only advisory checks failed — treat as green and
                    // fall through to the merge-eligibility path below.
                }

                // Only cache SHA once all checks have completed successfully.
                // If checks are still running, don't cache so we re-check
                // next tick.
                if all_completed {
                    self.pr_status_cache
                        .insert(task.id.clone(), current_sha.clone());
                    // All required checks passed (or only advisory failed).
                    self.persist_ci_snapshot(
                        &task.id,
                        pull_number,
                        &current_sha,
                        CiStatus::Passing,
                        vec![],
                        None,
                        0,
                        None,
                    )
                    .await;
                } else {
                    // Checks still running — persist pending status.
                    self.persist_ci_snapshot(
                        &task.id,
                        pull_number,
                        &current_sha,
                        CiStatus::Pending,
                        vec![],
                        None,
                        0,
                        None,
                    )
                    .await;
                }
            }

            // ── Merge eligibility check ───────────────────────────────────────
            // No changes requested, CI is green. Check if mergeable and approved.

            if pr.mergeable == Some(false) {
                // Clean-merge fast path (offloaded): try to resolve the conflict
                // mechanically in a background task before dispatching any agent.
                // `Merged` → PR refreshed, skip reopen; `InFlight` → skip this
                // tick (heavy merge must not block the coordinator); `Reopen` →
                // fall through to the agent rework flow.
                match self.poll_auto_merge_fast_path(&task.id, &task.short_id, &task.project_id) {
                    AutoMergeFastPathState::Merged => {
                        self.pr_status_cache.remove(&task.id);
                        self.review_stuck_sha_first_seen.remove(&task.id);
                        self.merge_fail_count.remove(&task.id);
                        self.delegated_to_github.remove(&task.id);
                        self.conversations_resolved.remove(&task.id);
                        continue;
                    }
                    AutoMergeFastPathState::InFlight => continue,
                    AutoMergeFastPathState::Reopen => {}
                }
                tracing::info!(
                    task_id = %task.short_id,
                    pr = pull_number,
                    "PR poller: PR has merge conflicts → reopening task for rework"
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
                self.review_stuck_sha_first_seen.remove(&task.id);
                self.merge_fail_count.remove(&task.id);
                self.delegated_to_github.remove(&task.id);
                self.conversations_resolved.remove(&task.id);
                continue;
            }

            // `has_approved` was already computed (effective, latest-per-reviewer)
            // alongside `changes_requested` above. A merge-gating review exists
            // when some reviewer's latest standing review is APPROVED or
            // CHANGES_REQUESTED; since CHANGES_REQUESTED is handled and `continue`d
            // above, by this point that reduces to `has_approved`. COMMENTED /
            // DISMISSED / PENDING carry no standing and never block auto-merge.
            let has_reviews = changes_requested || has_approved;

            if has_reviews && !has_approved {
                // Merge-gating reviews exist but none APPROVED (and no CHANGES_REQUESTED
                // handled above). Wait for approval.
                self.maybe_re_request_review(
                    &task.id,
                    &task.short_id,
                    gh_client,
                    &owner,
                    &repo,
                    pull_number,
                )
                .await;
                continue;
            }

            // ── Branch up-to-date check ───────────────────────────────────────
            // GitHub reports `mergeable_state == "behind"` when there are no
            // conflicts (`mergeable == true`) but branch protection requires
            // the head to include the latest base. Calling update-branch
            // merges base → head (the equivalent of clicking GitHub's
            // "Update branch" button), which bumps the head SHA and triggers
            // a fresh CI run. We bail out of this tick and let the poller
            // re-evaluate next time around once the new SHA settles.
            if pr.mergeable_state.as_deref() == Some("behind") {
                tracing::info!(
                    task_id = %task.short_id,
                    pr = pull_number,
                    "PR poller: PR is behind base — calling update-branch and retrying next tick"
                );
                match gh_client
                    .update_pull_request_branch(&owner, &repo, pull_number, &current_sha)
                    .await
                {
                    Ok(_) => {
                        // Head SHA will change; invalidate the CI cache so
                        // next tick re-checks against the new commit, and
                        // reset merge_fail_count since we're not actually
                        // attempting a merge this tick. Clear the
                        // delegated-to-GitHub marker too — the queue
                        // entry was pinned to the prior SHA and is now
                        // invalidated by update-branch.
                        self.pr_status_cache.remove(&task.id);
                        self.review_stuck_sha_first_seen.remove(&task.id);
                        self.merge_fail_count.remove(&task.id);
                        self.delegated_to_github.remove(&task.id);
                        self.conversations_resolved.remove(&task.id);
                    }
                    Err(e) => {
                        let github_error =
                            render_github_write_error("GitHub update-branch failed", &e);
                        // The GitHub API refused (race on expected head SHA,
                        // permissions, transient). Don't just warn-and-spin —
                        // fall back to the same mechanical merge the conflict
                        // path uses: fetch the mirror fresh, merge target →
                        // task branch in an ephemeral clone, push to mirror +
                        // GitHub. Clean merge → the PR refreshes exactly as
                        // update-branch would have done; real conflict → the
                        // next tick sees `dirty` and the conflict path (fast
                        // path, then ConflictRetry) takes over. Either way the
                        // PR can no longer sit `behind` forever on a wedged
                        // update-branch call.
                        tracing::warn!(
                            task_id = %task.short_id,
                            pr = pull_number,
                            error = %github_error,
                            "PR poller: update-branch failed — falling back to local mechanical merge (background)"
                        );
                        // Offloaded fallback: a clean background merge bumps the
                        // head exactly as update-branch would have. On `Merged`
                        // clear the per-SHA caches; `InFlight`/`Reopen` just wait
                        // for the next tick (a real conflict surfaces as `dirty`
                        // and the conflict path takes over then). Either way the
                        // PR can no longer sit `behind` forever, and the merge no
                        // longer blocks the tick.
                        if matches!(
                            self.poll_auto_merge_fast_path(
                                &task.id,
                                &task.short_id,
                                &task.project_id,
                            ),
                            AutoMergeFastPathState::Merged
                        ) {
                            self.pr_status_cache.remove(&task.id);
                            self.review_stuck_sha_first_seen.remove(&task.id);
                            self.merge_fail_count.remove(&task.id);
                            self.delegated_to_github.remove(&task.id);
                            self.conversations_resolved.remove(&task.id);
                        }
                    }
                }
                continue;
            }

            // ── CI reproduction pre-approval gate ───────────────────────────
            // Before auto-approving, re-run the repo-derived failing
            // required-check command for any failing durable CI snapshot.
            if let Some(workdir) = latest_task_workdir(&task.id, self.db.clone()).await {
                match run_ci_reproduction_preflight_gate(
                    &task,
                    &self.task_repo(),
                    gh_client,
                    &owner,
                    &repo,
                    &workdir,
                    CiPreflightGateKind::ReviewerApprove,
                )
                .await
                {
                    CiPreflightGateVerdict::Block { reason } => {
                        tracing::warn!(
                            task_id = %task.short_id,
                            pr = pull_number,
                            reason = %reason,
                            "PR poller: reviewer approval blocked by reproduced required-CI failure"
                        );
                        continue;
                    }
                    CiPreflightGateVerdict::RouteToLeadIntervention { reason } => {
                        tracing::warn!(
                            task_id = %task.short_id,
                            pr = pull_number,
                            reason = %reason,
                            "PR poller: unreproducible required-CI check routing to lead/human intervention"
                        );
                        let handled = self
                            .route_planner_intervention(
                                &task,
                                "worker",
                                &reason,
                                None,
                                task.reopen_count,
                            )
                            .await;
                        if handled {
                            tracing::info!(
                                task_id = %task.short_id,
                                pr = pull_number,
                                "PR poller: unreproducible CI check routed task to Planner intervention"
                            );
                        }
                        continue;
                    }
                    CiPreflightGateVerdict::Allow | CiPreflightGateVerdict::NotApplicable => {}
                }
            } else {
                tracing::debug!(
                    task_id = %task.short_id,
                    pr = pull_number,
                    "PR poller: no latest workspace path available; skipping reviewer CI reproduction preflight"
                );
            }

            // ── Auto-approve (per-user opt-in, with fallback approver) ────────
            // When some user has `auto_approve_prs=true` and we have their
            // live GitHub session token, POST an APPROVE review using their
            // identity. Branch protection's "1 approval required" gate is
            // what's left between us and the merge; the next poller tick
            // sees the new approval state and falls through to the merge
            // call naturally. Pinning `commit_id = current_sha` means a
            // fresh push automatically invalidates the approval.
            //
            // Approver selection (see `find_auto_approver_session`):
            //   1. The task's `created_by_user_id` if that user has the
            //      toggle on and a live session.
            //   2. Otherwise, any user with the toggle on and a live
            //      session — needed for tasks spawned by background
            //      agents (Planner/Architect/auto-breakdown) whose
            //      `created_by_user_id IS NULL`.
            //
            // Skipped silently when:
            //  - no user has the toggle on, or none has a live session
            //  - an approve attempt was already made on this exact SHA
            //  - we already have an approval (re-approving is a no-op anyway)
            if !has_approved
                && self
                    .auto_approve_attempted
                    .get(&task.id)
                    .is_none_or(|sha| sha != &current_sha)
                && let Some((user_id, session)) = self
                    .find_auto_approver_session(&task.id, &task.short_id)
                    .await
            {
                // Build a refreshable client when App OAuth creds are
                // visible to this process — that lets the transport
                // rotate the access/refresh pair transparently on 401
                // instead of asking the user to re-sign in every 8 hours.
                // If creds aren't available we degrade to the legacy
                // non-refreshable client; a 401 will then hard-evict the
                // session row and the next UI hit bounces to login.
                let user_client = match github_app_user::client_credentials_from_env() {
                    Some((cid, secret)) => GitHubApiClient::for_user_session(
                        session.github_access_token.clone(),
                        DbBackedRefresher::new(self.db.clone(), session.token.clone(), cid, secret)
                            .into_arc(),
                    ),
                    None => {
                        tracing::debug!(
                            "PR poller: GITHUB_APP_CLIENT_ID/SECRET unset; \
                             constructing non-refreshable user client"
                        );
                        GitHubApiClient::for_user_token(session.github_access_token.clone())
                    }
                };
                match user_client
                    .approve_pull_request(&owner, &repo, pull_number, &current_sha)
                    .await
                {
                    Ok(()) => {
                        tracing::info!(
                            task_id = %task.short_id,
                            user_id = %user_id,
                            pr = pull_number,
                            sha = %current_sha,
                            "PR poller: auto-approved PR on user's behalf"
                        );
                        self.auto_approve_attempted
                            .insert(task.id.clone(), current_sha.clone());
                        self.pr_status_cache.remove(&task.id);
                        continue;
                    }
                    Err(e) => {
                        // Suppress regardless of failure mode — same SHA
                        // shouldn't be retried until a fresh push lands.
                        self.auto_approve_attempted
                            .insert(task.id.clone(), current_sha.clone());
                        if e.downcast_ref::<UserTokenExpired>().is_some() {
                            tracing::info!(
                                task_id = %task.short_id,
                                user_id = %user_id,
                                pr = pull_number,
                                "PR poller: auto-approve skipped — user token expired (sign in to re-arm)"
                            );
                        } else {
                            tracing::warn!(
                                task_id = %task.short_id,
                                user_id = %user_id,
                                pr = pull_number,
                                error = %e,
                                "PR poller: auto-approve failed; falling through to wait for human approval"
                            );
                        }
                        // Fall through to the merge attempt below —
                        // GitHub will reject if branch protection requires
                        // an approval, which surfaces as a normal merge
                        // failure in the existing path.
                    }
                }
            }

            // ── Merge path: delegated-to-GitHub vs legacy direct merge ────────
            // Three modes, picked in order:
            //
            //   (a) GitHub already owns the PR's merge timing — either
            //       `enablePullRequestAutoMerge` succeeded earlier (`pr.auto_merge`
            //       is set) OR we explicitly enqueued the PR into the merge
            //       queue on a previous tick (tracked in `delegated_to_github`
            //       keyed by SHA so a new push re-enters this branch). Just
            //       observe; on `UNMERGEABLE` or a failure-flavored dequeue
            //       event the observer surfaces `PrCiFailed`.
            //
            //   (b) REST `PUT /pulls/{n}/merge` succeeds — repos without
            //       merge-queue branch protection. Close the task.
            //
            //   (c) REST returns the merge-queue 405 — repo enforces a
            //       merge queue. We call `enqueuePullRequest` directly
            //       (works regardless of the repo's "Allow auto-merge"
            //       setting; `enable_auto_merge_best_effort` from undraft
            //       time often hits `UNPROCESSABLE` and is unreliable).
            //       Then mark delegated and observe next tick.
            let delegated_for_current_sha = self
                .delegated_to_github
                .get(&task.id)
                .is_some_and(|sha| sha == &current_sha);
            if pr.auto_merge.is_some() || delegated_for_current_sha {
                self.observe_auto_merge_state(
                    gh_client,
                    &task.id,
                    &task.short_id,
                    pr_url,
                    &owner,
                    &repo,
                    pull_number,
                    &pr.node_id,
                    has_approved,
                    &current_sha,
                )
                .await;
                continue;
            }
            // SHA moved since we last delegated — drop the stale entry so a
            // fresh enqueue attempt fires below. Drop the conversation-resolved
            // marker too: the new commit may have new review threads, and the
            // SHA-keyed guard would otherwise hold the old SHA harmlessly, but
            // clearing keeps the map tidy.
            if self.delegated_to_github.contains_key(&task.id) {
                self.delegated_to_github.remove(&task.id);
                self.conversations_resolved.remove(&task.id);
            }

            // ── CI merge gate ───────────────────────────────────────────────
            // Block Djinn-initiated merge/close unless the durable CI snapshot
            // for the current PR head is `passing`. This prevents merge when:
            //   - Required checks are still running (pending/unknown) → hold
            //   - Required checks are failing → block (remediation handles)
            //   - Snapshot is stale (head SHA mismatch) → hold
            //   - No snapshot exists yet → hold
            //
            // The "PR already merged" observation path (pr.merged == Some(true)
            // above) is intentionally NOT gated — that records an external
            // merge, not a Djinn-initiated one.
            {
                let ci_snapshot = match self
                    .task_repo()
                    .get_ci_snapshot_for_task_pr(&task.id, pr_number_i64)
                    .await
                {
                    Ok(snap) => snap,
                    Err(e) => {
                        tracing::warn!(
                            task_id = %task.short_id,
                            pr = pull_number,
                            error = %e,
                            "PR poller: failed to read CI snapshot — blocking merge conservatively"
                        );
                        continue;
                    }
                };
                match ci_merge_gate_verdict(ci_snapshot.as_ref(), &current_sha) {
                    CiMergeGateVerdict::Allow => { /* fall through to merge */ }
                    CiMergeGateVerdict::Hold => {
                        tracing::info!(
                            task_id = %task.short_id,
                            pr = pull_number,
                            ci_status = ?ci_snapshot.as_ref().map(|s| s.ci_status),
                            snapshot_sha = ?ci_snapshot.as_ref().map(|s| &s.head_sha),
                            current_sha = %current_sha,
                            "PR poller: CI merge gate: holding — not yet passing on current head"
                        );
                        continue;
                    }
                    CiMergeGateVerdict::Block => {
                        tracing::info!(
                            task_id = %task.short_id,
                            pr = pull_number,
                            ci_status = "failing",
                            "PR poller: CI merge gate: blocking — required CI failing on current head"
                        );
                        continue;
                    }
                }
            }

            // ── Tripwire active-hold gate (pre-merge boundary) ────────────
            // After CI passes and before any Djinn-initiated merge or
            // enqueue, check the durable active-hold state for the current
            // head SHA.  If an active hold exists (unreleased enforcement
            // findings from a prior tripwire gate evaluation), the PR must
            // NOT merge even when CI is green.  The reconciliation helper
            // also detects and reapplies missing human-review-hold labels
            // (label tamper), failing closed on errors.
            //
            // The "PR already merged" observation path (pr.merged == Some(true)
            // above) is intentionally NOT gated — that records a historical
            // external merge, not a Djinn-initiated one.  The delegated
            // observation path (auto_merge / delegated_for_current_sha)
            // observes state already under GitHub's control; if the PR was
            // delegated before a hold was established, GitHub controls the
            // outcome.
            if self
                .reconcile_tripwire_hold(&task, pull_number, &current_sha)
                .await
            {
                tracing::info!(
                    task_id = %task.short_id,
                    pr = pull_number,
                    head_sha = %current_sha,
                    "PR poller: tripwire active-hold gate: blocking merge — active hold on current head"
                );
                continue;
            }

            // Either approved or no reviews — attempt squash merge.
            tracing::info!(
                task_id = %task.short_id,
                pr = pull_number,
                approved = has_approved,
                "PR poller: attempting squash merge"
            );

            match gh_client
                .merge_pull_request(&owner, &repo, pull_number, MergeMethod::Squash, &pr.title)
                .await
            {
                Ok(merge_response) => {
                    // The PUT /merge response carries the landed squash-commit
                    // SHA (`{"sha": ..., "merged": true}`); record it so the
                    // task lands in the board's Merged column.
                    let merge_commit_sha = merge_response
                        .get("sha")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned);
                    tracing::info!(
                        task_id = %task.short_id,
                        pr = pull_number,
                        sha = merge_commit_sha.as_deref().unwrap_or("<unknown>"),
                        "PR poller: squash merge succeeded → closing task"
                    );
                    self.apply_pr_merge(&task.id, merge_commit_sha.as_deref())
                        .await;
                    self.pr_status_cache.remove(&task.id);
                    self.review_stuck_sha_first_seen.remove(&task.id);
                    self.merge_fail_count.remove(&task.id);
                    self.delegated_to_github.remove(&task.id);
                    self.handled_dequeues.remove(&task.id);
                }
                Err(e) => {
                    // Merge-queue 405: the repo's branch protection routes
                    // everything through a merge queue. The REST merge
                    // endpoint is not allowed here — directly enqueue the
                    // PR via GraphQL `enqueuePullRequest`, which works
                    // regardless of the repo's "Allow auto-merge" setting.
                    if is_merge_queue_405(&e) {
                        match gh_client
                            .enqueue_pull_request(&pr.node_id, &current_sha)
                            .await
                        {
                            Ok(()) => {
                                tracing::info!(
                                    task_id = %task.short_id,
                                    pr = pull_number,
                                    sha = %current_sha,
                                    "PR poller: enqueued PR into merge queue — switching to observe mode"
                                );
                                self.delegated_to_github
                                    .insert(task.id.clone(), current_sha.clone());
                                self.merge_fail_count.remove(&task.id);
                            }
                            // "Pull request is already in the queue": someone
                            // else enqueued it (GitHub merge-when-ready, or a
                            // pre-restart delegation this process no longer
                            // remembers). The queue entry is the state we
                            // wanted — adopt it and switch to observe mode.
                            // Without this the poller retries enqueue every
                            // tick and never reaches `observe_auto_merge_state`,
                            // so a failure dequeue for the queued head would go
                            // unhandled until GitHub evicts the PR again.
                            Err(enqueue_err) if is_already_queued(&enqueue_err) => {
                                tracing::info!(
                                    task_id = %task.short_id,
                                    pr = pull_number,
                                    sha = %current_sha,
                                    "PR poller: PR already in merge queue — adopting entry, switching to observe mode"
                                );
                                self.delegated_to_github
                                    .insert(task.id.clone(), current_sha.clone());
                                self.merge_fail_count.remove(&task.id);
                            }
                            Err(enqueue_err) => {
                                let github_error = render_github_write_error(
                                    "GitHub enqueue PR failed",
                                    &enqueue_err,
                                );
                                // Enqueue failed (PR not ready: missing
                                // approval, failing checks, etc.). Don't
                                // mark delegated — next tick re-checks
                                // upstream gates and tries again.
                                //
                                // One concrete cause on merge-queue repos with
                                // the "conversation must be resolved" rule is
                                // unresolved review threads: `enqueuePullRequest`
                                // rejects the PR much like the direct-merge 405
                                // does. Unlike that REST path the rejection comes
                                // back as a GraphQL error (no "405" to match on),
                                // so we don't string-sniff it — instead, on an
                                // APPROVED PR (CI already gated green above), we
                                // resolve any leftover threads directly. Same
                                // policy as the auto-merge + direct-merge paths;
                                // harmless if conversations weren't the blocker
                                // (the resolve list comes back empty). The
                                // SHA-keyed `conversations_resolved` guard stops
                                // us re-resolving every tick when a different gate
                                // keeps enqueue failing.
                                if has_approved
                                    && self.conversations_resolved.get(&task.id)
                                        != Some(&current_sha)
                                    && let Some(resolved) = self
                                        .resolve_unresolved_conversations(
                                            gh_client,
                                            &owner,
                                            &repo,
                                            pull_number,
                                            &task.short_id,
                                        )
                                        .await
                                {
                                    self.conversations_resolved
                                        .insert(task.id.clone(), current_sha.clone());
                                    if resolved > 0 {
                                        tracing::info!(
                                            task_id = %task.short_id,
                                            pr = pull_number,
                                            resolved,
                                            "PR poller: approved PR rejected from merge queue on unresolved conversations — resolved threads, will retry enqueue next tick"
                                        );
                                        self.merge_fail_count.remove(&task.id);
                                        continue;
                                    }
                                }
                                // Bump merge_fail_count so the cache-invalidate
                                // threshold still kicks in on persistent
                                // failure.
                                let count =
                                    self.merge_fail_count.entry(task.id.clone()).or_insert(0);
                                *count += 1;
                                tracing::warn!(
                                    task_id = %task.short_id,
                                    pr = pull_number,
                                    attempt = *count,
                                    error = %github_error,
                                    "PR poller: enqueue_pull_request failed (will retry next tick)"
                                );
                                if *count >= MERGE_RETRY_RECHECK_THRESHOLD {
                                    tracing::info!(
                                        task_id = %task.short_id,
                                        pr = pull_number,
                                        "PR poller: {} consecutive enqueue failures, invalidating CI cache for re-check",
                                        *count
                                    );
                                    self.pr_status_cache.remove(&task.id);
                                    *count = 0;
                                }
                            }
                        }
                        continue;
                    }

                    // Conversation-resolution 405: the repo's branch protection
                    // requires every review conversation to be resolved before
                    // merge, and an external review bot approved the PR *and*
                    // left inline comments → unresolved threads. An explicit
                    // approval is the override signal: the reviewer's comments
                    // are non-blocking, so resolve the leftover threads and let
                    // the next tick re-attempt the merge. Only do this when the
                    // PR is approved — otherwise this rule is a legitimate gate.
                    if has_approved && is_conversation_resolution_block(&e) {
                        match self
                            .resolve_unresolved_conversations(
                                gh_client,
                                &owner,
                                &repo,
                                pull_number,
                                &task.short_id,
                            )
                            .await
                        {
                            // Resolved at least one thread → idempotent and
                            // converging. Reset the fail count and retry the
                            // merge next tick.
                            Some(resolved) if resolved > 0 => {
                                tracing::info!(
                                    task_id = %task.short_id,
                                    pr = pull_number,
                                    resolved,
                                    "PR poller: approved PR blocked on unresolved conversations — resolved threads, will retry merge next tick"
                                );
                                self.merge_fail_count.remove(&task.id);
                                continue;
                            }
                            // Nothing unresolved (Some(0)) yet GitHub still
                            // blocked, or we couldn't list them (None): do NOT
                            // continue. Fall through to the generic
                            // merge_fail_count path so a PR blocked for some
                            // OTHER reason can't spin forever through this branch.
                            _ => {}
                        }
                    }

                    let github_error = render_github_write_error("GitHub PR merge failed", &e);
                    let count = self.merge_fail_count.entry(task.id.clone()).or_insert(0);
                    *count += 1;
                    tracing::warn!(
                        task_id = %task.short_id,
                        pr = pull_number,
                        attempt = *count,
                        error = %github_error,
                        "PR poller: merge failed (will retry next tick)"
                    );
                    // After repeated failures, invalidate the CI cache so the
                    // next tick re-checks whether checks actually passed.
                    // This catches the case where CI failed after we cached
                    // a "green" SHA.
                    if *count >= MERGE_RETRY_RECHECK_THRESHOLD {
                        tracing::info!(
                            task_id = %task.short_id,
                            pr = pull_number,
                            "PR poller: {} consecutive merge failures, invalidating CI cache for re-check",
                            *count
                        );
                        self.pr_status_cache.remove(&task.id);
                        *count = 0;
                    }
                }
            }
        }
    }
}
