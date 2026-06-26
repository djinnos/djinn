use super::*;

impl CoordinatorActor {
    pub(crate) async fn resolve_required_contexts(
        &self,
        gh_client: &GitHubApiClient,
        owner: &str,
        repo: &str,
        base_ref: &str,
        pull_number: u64,
    ) -> Option<Vec<String>> {
        match gh_client
            .required_check_contexts_for_pr(owner, repo, pull_number)
            .await
        {
            Ok(Some(contexts)) => return Some(contexts),
            Ok(None) => {
                tracing::info!(
                    pr = pull_number,
                    "PR poller: no status-check rollup on PR head — falling back to branch-protection required checks"
                );
            }
            Err(e) => {
                tracing::info!(
                    pr = pull_number,
                    error = %e,
                    "PR poller: could not read per-PR required checks via GraphQL — falling back to branch-protection required checks"
                );
            }
        }
        gh_client
            .list_required_status_checks(owner, repo, base_ref)
            .await
            .ok()
            .flatten()
    }

    /// Shared handler for a "CI checks failed on PR" event, used by both the
    /// `pr_draft` and `pr_review` polling paths.
    ///
    /// Fixes the infinite CI-failure rework loop by gating the rework on five
    /// things, in order:
    ///
    /// 1. **Blocking-only filter.** The failed check-runs are intersected with
    ///    the PR's *required* status-check contexts, resolved via
    ///    [`Self::resolve_required_contexts`] (GitHub's per-PR `isRequired`
    ///    answer — branch protection, rulesets, and merge queue alike).
    ///    Advisory/preview checks (Vercel deploys, preview-env provisioning,
    ///    a repo's non-required `Sentinel` bot gate, etc.) are dropped: a code
    ///    diff cannot fix that infra, so reworking on it loops forever. When no
    ///    source can answer we fall back to a conservative name-pattern
    ///    heuristic. If nothing blocking remains, we do **not** reopen — the
    ///    required checks are green and the PR is fine to proceed.
    ///
    /// 2. **Same-CI-signature check.** A stable fingerprint is computed from the
    ///    blocking check names and structured CI failure sections. If the same
    ///    fingerprint appears `SAME_CI_SIGNATURE_THRESHOLD` times in a row,
    ///    we escalate to the Planner via `route_planner_intervention` faster
    ///    than the blind cycle-count threshold. The counter resets when the
    ///    fingerprint changes (different failures = progress).
    ///
    /// 3. **Scope-inversion check.** The PR's actual changed files are fetched
    ///    from GitHub and compared against the failing crates/files extracted
    ///    from the CI failure sections. If CI fails on crates outside the PR's
    ///    own diff, this is a decomposition error (too-narrow slice), not a worker
    ///    bug. We route to the Planner for a RE-SLICE instead of re-dispatching
    ///    the worker.
    ///
    /// 4. **Diff-empty short-circuit.** If the PR head has no commits ahead of
    ///    base on GitHub (`ahead_by == 0`), the previous worker iteration
    ///    produced no new diff — re-dispatching cannot change anything. We
    ///    escalate to the Planner and PARK (hold the source on the remediation
    ///    blocker) instead of looping.
    ///
    /// 5. **Cycle cap.** Each CI-failure rework records a `pr_ci_cycle` marker.
    ///    Past `PR_CI_FAILURE_THRESHOLD` we escalate to the Planner and PARK
    ///    rather than redispatch. Escalation is terminal — the counter is never
    ///    reset on the reopen that re-arms the loop.
    ///
    /// Returns `true` when the event was *consumed* (the task was transitioned
    /// — either reworked or parked on a remediation blocker) and the caller
    /// should run its post-transition cache cleanup and `continue`. Returns
    /// `false` when the
    /// failures were all non-blocking and the caller should fall through to its
    /// normal (CI-passed) handling.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn handle_ci_failure(
        &mut self,
        gh_client: &GitHubApiClient,
        task: &djinn_core::models::Task,
        pr: &PullRequest,
        failed_checks: &[&CheckRun],
        pr_url: &str,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) -> bool {
        let task_id = &task.id;
        let task_short_id = &task.short_id;
        let current_sha = &pr.head.sha;
        let base_ref = &pr.base.ref_name;

        // ── 1. Filter to blocking (required) failures ─────────────────────────
        // Resolve the PR's required check contexts (GitHub's own per-PR
        // `isRequired` answer, falling back to branch protection then the
        // advisory-name heuristic). `None` → heuristic; an authoritative empty
        // set → nothing is required, so no failure is blocking.
        let required_contexts: Option<Vec<String>> = self
            .resolve_required_contexts(gh_client, owner, repo, base_ref, pull_number)
            .await;

        let blocking = blocking_failed_checks(failed_checks, required_contexts.as_deref());

        if blocking.is_empty() {
            tracing::info!(
                task_id = %task_short_id,
                pr = pull_number,
                sha = %current_sha,
                failed_count = failed_checks.len(),
                "PR poller: all failed checks are non-blocking (advisory/preview); \
                 required checks are green — not reopening for rework"
            );
            return false;
        }

        // Pre-compute CI failure sections for potential use in terminal
        // escalation paths. This mirrors `log_ci_failure_comment`: parse every
        // distinct Actions run id from the failed check-runs, fetch run jobs,
        // de-duplicate jobs by id, and let `build_ci_failure_sections` fall back
        // to raw check-run names if no job data could be fetched. Use only the
        // blocking (required) failures here so the escalation details match the
        // reason that will be sent to the Planner.
        let ci_failure_sections: Vec<String> = {
            let mut run_ids: Vec<u64> = Vec::new();
            for cr in failed_checks {
                if let Some(rid) = parse_actions_run_id(&cr.html_url)
                    && !run_ids.contains(&rid)
                {
                    run_ids.push(rid);
                }
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
            let (sections, _ci_jobs) = build_ci_failure_sections(jobs, &blocking);
            sections
        };

        // ── 2. Same-CI-signature check (content-aware escalation) ───────────────
        // Compute a fingerprint from the blocking failures + structured CI
        // sections. If the same fingerprint appears K times in a row, escalate
        // to the Planner faster than the blind cycle-count threshold.
        let fingerprint = compute_ci_failure_fingerprint(&blocking, &ci_failure_sections);

        let task_repo = self.task_repo();
        let prior_signatures = match task_repo
            .query_activity(ActivityQuery {
                task_id: Some(task_id.to_owned()),
                event_type: Some(SAME_CI_SIGNATURE_EVENT.to_string()),
                actor_role: Some("system".to_string()),
                project_id: None,
                from_time: None,
                to_time: None,
                limit: 100,
                offset: 0,
            })
            .await
        {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!(
                    task_id = %task_short_id,
                    error = %e,
                    "PR poller: failed to query same_ci_signature markers; assuming none"
                );
                Vec::new()
            }
        };

        let consecutive = count_consecutive_identical(&prior_signatures, &fingerprint);
        // The current failure is the (consecutive + 1)th identical fingerprint.
        let total_consecutive = consecutive + 1;

        if total_consecutive >= SAME_CI_SIGNATURE_THRESHOLD {
            let blocking_names: Vec<&str> = blocking.iter().map(|cr| cr.name.as_str()).collect();
            let reason = format!(
                "PR #{pull_number}: CI has failed with the identical fingerprint {total_consecutive} \
                 consecutive times (checks: {check_names}). The worker is not making progress on \
                 these specific failures. Escalating for planner intervention.",
                check_names = blocking_names.join(", "),
            );
            tracing::warn!(
                task_id = %task_short_id,
                pr = pull_number,
                fingerprint = %fingerprint,
                consecutive = total_consecutive,
                threshold = SAME_CI_SIGNATURE_THRESHOLD,
                "PR poller: same CI failure signature detected — escalating for planner intervention"
            );
            let sections_text = ci_failure_sections.join("\n");
            self.route_planner_intervention(task, "worker", &reason, Some(&sections_text))
                .await;
            // The intervention escalated + blocked the source; park it to `open`
            // so it is genuinely HELD (not left in pr_draft/pr_review where the
            // poller keeps re-polling the red PR) and revivable when the
            // remediation closes.
            self.park_source_open(task_id, &reason).await;
            return true;
        }

        // Record this fingerprint marker so future rounds can detect repeats.
        let signature_payload = serde_json::json!({
            "fingerprint": fingerprint,
            "round": total_consecutive,
            "check_names": blocking.iter().map(|cr| cr.name.as_str()).collect::<Vec<&str>>(),
        })
        .to_string();
        if let Err(e) = task_repo
            .log_activity(
                Some(task_id),
                "coordinator",
                "system",
                SAME_CI_SIGNATURE_EVENT,
                &signature_payload,
            )
            .await
        {
            tracing::warn!(
                task_id = %task_short_id,
                error = %e,
                "PR poller: failed to store same_ci_signature marker"
            );
        }

        // ── 3. Scope-inversion check ────────────────────────────────────────────
        // If CI is failing on crates/files that the PR never touched, this is a
        // decomposition error (too-narrow slice), not a worker bug. Route to the
        // Planner for a RE-SLICE instead of re-dispatching the worker.
        let pr_files = match gh_client.get_pr_files(owner, repo, pull_number).await {
            Ok(files) => files.iter().map(|f| f.filename.clone()).collect::<Vec<_>>(),
            Err(e) => {
                tracing::warn!(
                    task_id = %task_short_id,
                    error = %e,
                    "PR poller: failed to fetch PR files for scope-inversion check; skipping"
                );
                Vec::new() // empty → detect_scope_inversion returns None
            }
        };

        if let Some(true) = detect_scope_inversion(&ci_failure_sections, &pr_files) {
            let pr_crates = extract_crate_names(&pr_files);
            let failing_crates = extract_crate_names_from_sections(&ci_failure_sections);
            let reason = format!(
                "PR #{pull_number} scope-inversion detected: CI is failing on crates/files \
                 that are NOT in this PR's own diff (PR touches: {pr_crates}; CI fails on: \
                 {failing_crates}). This is likely a decomposition error — the task was sliced \
                 too narrowly. RE-SLICE: expand the task scope to include the missing crate(s), \
                 or split into a separate task that covers the failing area.",
                pr_crates = pr_crates.join(", "),
                failing_crates = failing_crates.join(", "),
            );
            let sections_text = ci_failure_sections.join("\n");
            self.route_planner_intervention(task, "worker", &reason, Some(&sections_text))
                .await;
            // Park the source to `open` so it is held by the blocker the
            // intervention added (not left in pr_draft/pr_review) and revivable
            // when the remediation closes.
            self.park_source_open(task_id, &reason).await;
            return true;
        }
        // If Some(false): normal worker bug, fall through to existing retry path.
        // If None: inconclusive, fall through to existing retry path.

        // ── 4. Diff-empty short-circuit ───────────────────────────────────────
        // If the head has no commits ahead of base, the last worker iteration
        // produced no new diff. Re-dispatching cannot change the outcome, so
        // escalate + force-close instead of looping on the same SHA.
        match gh_client
            .compare_commits_ahead_by(owner, repo, base_ref, current_sha)
            .await
        {
            Ok(0) => {
                let blocking_names: Vec<&str> =
                    blocking.iter().map(|cr| cr.name.as_str()).collect();
                let reason = format!(
                    "PR #{pull_number} stuck: required checks keep failing ({}) but the task branch \
                     has no commits ahead of `{base_ref}` (head `{sha}`) — the worker produced no new \
                     diff, so re-running cannot fix it. Escalating for human attention.",
                    blocking_names.join(", "),
                    sha = &current_sha[..current_sha.len().min(12)],
                );
                tracing::warn!(
                    task_id = %task_short_id,
                    pr = pull_number,
                    sha = %current_sha,
                    "PR poller: CI failed but branch is diff-empty vs base — escalating + parking (held on remediation blocker)"
                );
                self.escalate_ci_failure_and_park(task, pr_url, &reason, &ci_failure_sections)
                    .await;
                return true;
            }
            Ok(_) => {}
            Err(e) => {
                // Don't block the rework on a compare-API failure — fall
                // through to the cycle-cap path, which still bounds the loop.
                tracing::info!(
                    task_id = %task_short_id,
                    pr = pull_number,
                    error = %e,
                    "PR poller: compare-commits precheck failed; proceeding with cycle-cap path"
                );
            }
        }

        // ── 5. Cycle cap ──────────────────────────────────────────────────────
        let prior_cycles = match task_repo
            .query_activity(ActivityQuery {
                task_id: Some(task_id.to_owned()),
                event_type: Some(PR_CI_CYCLE_EVENT.to_string()),
                actor_role: Some("system".to_string()),
                project_id: None,
                from_time: None,
                to_time: None,
                limit: 100,
                offset: 0,
            })
            .await
        {
            Ok(entries) => entries.len() as u32,
            Err(e) => {
                tracing::warn!(
                    task_id = %task_short_id,
                    error = %e,
                    "PR poller: failed to count CI-failure cycles; assuming 0"
                );
                0
            }
        };
        let round = prior_cycles + 1;

        if round > PR_CI_FAILURE_THRESHOLD {
            let blocking_names: Vec<&str> = blocking.iter().map(|cr| cr.name.as_str()).collect();
            let reason = format!(
                "PR #{pull_number} stuck: required checks ({}) have failed across {prior} CI-failure \
                 rework rounds (threshold {PR_CI_FAILURE_THRESHOLD}) without going green. The worker \
                 cannot resolve this on its own — escalating for human attention.",
                blocking_names.join(", "),
                prior = prior_cycles,
            );
            tracing::warn!(
                task_id = %task_short_id,
                pr = pull_number,
                round,
                threshold = PR_CI_FAILURE_THRESHOLD,
                "PR poller: CI-failure rework threshold exceeded — escalating + parking (held on remediation blocker)"
            );
            self.escalate_ci_failure_and_park(task, pr_url, &reason, &ci_failure_sections)
                .await;
            return true;
        }

        // Below threshold: record the cycle marker, surface CI feedback, and
        // reopen for a fresh worker iteration.
        let cycle_payload = serde_json::json!({ "round": round }).to_string();
        if let Err(e) = task_repo
            .log_activity(
                Some(task_id),
                "coordinator",
                "system",
                PR_CI_CYCLE_EVENT,
                &cycle_payload,
            )
            .await
        {
            tracing::warn!(
                task_id = %task_short_id,
                error = %e,
                "PR poller: failed to store pr_ci_cycle marker"
            );
        }

        tracing::info!(
            task_id = %task_short_id,
            pr = pull_number,
            sha = %current_sha,
            round,
            threshold = PR_CI_FAILURE_THRESHOLD,
            blocking_count = blocking.len(),
            "PR poller: required CI check failed on PR → reopening task for rework (round {}/{})",
            round,
            PR_CI_FAILURE_THRESHOLD,
        );
        self.apply_pr_transition(
            task_id,
            TransitionAction::PrCiFailed,
            Some("CI checks failed on PR"),
        )
        .await;
        // Non-required failures ride along as informational context so the
        // rework worker knows about them without treating them as blockers.
        let blocking_names: std::collections::HashSet<&str> =
            blocking.iter().map(|cr| cr.name.as_str()).collect();
        let advisory: Vec<&CheckRun> = failed_checks
            .iter()
            .filter(|cr| !blocking_names.contains(cr.name.as_str()))
            .copied()
            .collect();
        self.log_ci_failure_comment(
            task_id,
            &blocking,
            &advisory,
            pr_url,
            current_sha,
            gh_client,
            owner,
            repo,
        )
        .await;
        true
    }

    /// Park (HOLD) a CI-failure loop the worker can't resolve (diff-empty
    /// re-emit, or cycle cap exceeded). Logs a visibility comment, dispatches a
    /// Planner remediation (which creates a remediation task and BLOCKS the
    /// source on it), then parks the source back to `open` so it is held by that
    /// blocker — NOT force-closed. `list_ready` filters the blocked-open task out
    /// of dispatch (no slot consumed) and `emit_unblocked_tasks` revives it the
    /// moment the remediation closes. Never resets the CI-cycle counter.
    pub(crate) async fn escalate_ci_failure_and_park(
        &mut self,
        task: &djinn_core::models::Task,
        pr_url: &str,
        reason: &str,
        ci_failure_sections: &[String],
    ) {
        let task_repo = self.task_repo();

        let sections_text = if ci_failure_sections.is_empty() {
            String::new()
        } else {
            format!(
                "\n\n**CI Failure Details:**\n{}",
                ci_failure_sections.join("\n")
            )
        };
        let comment_body = format!("**PR CI Escalation**: {reason}{sections_text}\n\nPR: {pr_url}");
        let comment_payload = serde_json::json!({ "body": comment_body }).to_string();
        if let Err(e) = task_repo
            .log_activity(
                Some(&task.id),
                "coordinator",
                "system",
                "comment",
                &comment_payload,
            )
            .await
        {
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                "PR poller: failed to log CI escalation comment"
            );
        }

        // Escalate to the Planner (ADR-051 §8 escalation ceiling), which creates
        // a remediation task and BLOCKS the source on it, so a human / Planner
        // sees why the task gave up and can drive it forward.
        let enriched_reason = if ci_failure_sections.is_empty() {
            reason.to_string()
        } else {
            format!(
                "{reason}\n\n**CI Failure Details:**\n{}",
                ci_failure_sections.join("\n")
            )
        };
        self.dispatch_planner_escalation(&task.id, &enriched_reason, &task.project_id)
            .await;

        // Park (hold) the source on the blocker just added — NOT force-close.
        // The blocker was added by `dispatch_planner_escalation` BEFORE this
        // park, so the open task is never dispatchable without its blocker.
        self.park_source_open(&task.id, reason).await;
        self.pr_status_cache.remove(&task.id);
        self.pr_draft_first_seen.remove(&task.id);
        self.review_stuck_sha_first_seen.remove(&task.id);
        self.merge_fail_count.remove(&task.id);
        self.delegated_to_github.remove(&task.id);
        self.conversations_resolved.remove(&task.id);
    }

    /// Park a stuck source task: move it to `open` so it is HELD by the
    /// remediation blocker that the escalation path already added — never
    /// closed, never left in `pr_draft`/`pr_review` (where the PR poller would
    /// keep re-polling its red PR). A no-op when the task is already `open`.
    ///
    /// Ordering contract: callers MUST add the remediation blocker BEFORE
    /// calling this, so there is no window where the open task is dispatchable
    /// without its blocker. Once parked, `list_ready` filters it out (blocked)
    /// and `emit_unblocked_tasks` revives it when the remediation closes.
    pub(crate) async fn park_source_open(&self, task_id: &str, reason: &str) {
        self.apply_pr_transition(task_id, TransitionAction::ParkForRemediation, Some(reason))
            .await;
    }
}
pub(crate) fn is_merge_queue_405(
    err: &(impl crate::github_error_render::GithubWriteError + ?Sized),
) -> bool {
    crate::github_error_render::github_write_status_is(err, 405)
        && crate::github_error_render::github_write_body_contains(err, "merge queue")
}

/// Detect the `enqueuePullRequest` UNPROCESSABLE rejection whose message is
/// "Pull request is already in the queue" — the entry we wanted already
/// exists (GitHub merge-when-ready armed it, or a pre-restart delegation this
/// process no longer remembers). Callers adopt the entry instead of erroring.
pub(crate) fn is_already_queued(
    err: &(impl crate::github_error_render::GithubWriteError + ?Sized),
) -> bool {
    crate::github_error_render::github_write_body_contains(err, "already in the queue")
}

/// Heuristic: is this check-run name an *advisory* (non-merge-gating) check?
///
/// Used only as a fallback when we couldn't read the branch's required-status-
/// check contexts from branch protection (no protection configured, or the
/// installation lacks the permission). Matches the common preview/deploy
/// integrations whose failures cannot be fixed by a code diff (Vercel/Netlify
/// deploy previews, preview-environment provisioning, etc.).
///
/// Matching is case-insensitive and substring-based so it catches the various
/// per-app context names GitHub emits (e.g. `Vercel – acme-portal`,
/// `PR Preview Environment Setup / setup-preview`).
pub(crate) fn is_advisory_check_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    const ADVISORY_MARKERS: &[&str] = &[
        "vercel",
        "netlify",
        "preview",
        "deploy-preview",
        "deployment",
        "deploy /",
        "cloudflare pages",
        "render.com",
        "surge",
        "amplify",
    ];
    ADVISORY_MARKERS.iter().any(|m| lower.contains(m))
}

/// From the set of failed check-runs, return only those that are *blocking*
/// (merge-gating).
///
/// - When `required_contexts` is `Some` (we read the branch's required status
///   checks from branch protection / rulesets — the source of truth), keep:
///   1. failed checks whose name matches a required context directly, AND
///   2. failed checks that belong to the **same workflow run** as any required
///      failing check (same `/actions/runs/{id}/` in their `html_url`).
///
///   Rule (2) is the fix for aggregate gating checks. Repos increasingly gate
///   `main` on a single *aggregate* status (e.g. a `Quality Gate` job whose
///   `needs:` list fans out to `Server Clippy` / `Server Test` / …). Only the
///   aggregate is listed as a required context; the constituent jobs that
///   actually failed (and that a code diff CAN fix) are *not*. GitHub reports
///   every job in that workflow run as its own check-run sharing one run id, so
///   when a required aggregate is red we treat the failing jobs in its run as
///   the real blockers. This is general — it keys off "shares a run with a
///   failing required check," never a hardcoded job-name list.
///
///   Anything that is neither required nor part of a required run is advisory
///   (Vercel previews, an optional bot gate in its own workflow, …) and must
///   not trigger a rework.
/// - When `required_contexts` is `None` (branch protection unreadable), fall
///   back to the name-pattern heuristic: keep failed checks that are *not*
///   recognised as advisory. This is intentionally conservative — an unknown
///   check is treated as blocking so we never silently swallow a real failure.
pub(crate) fn blocking_failed_checks<'a>(
    failed: &[&'a CheckRun],
    required_contexts: Option<&[String]>,
) -> Vec<&'a CheckRun> {
    match required_contexts {
        Some(required) => {
            let is_required = |cr: &CheckRun| required.iter().any(|ctx| ctx == &cr.name);

            // The workflow-run ids that contain at least one failing *required*
            // check. The jobs of an aggregate gate live in this same run, so any
            // failing check sharing one of these run ids is a genuine blocker.
            let required_run_ids: std::collections::HashSet<u64> = failed
                .iter()
                .filter(|cr| is_required(cr))
                .filter_map(|cr| parse_actions_run_id(&cr.html_url))
                .collect();

            failed
                .iter()
                .filter(|cr| {
                    is_required(cr)
                        || parse_actions_run_id(&cr.html_url)
                            .is_some_and(|rid| required_run_ids.contains(&rid))
                })
                .copied()
                .collect()
        }
        None => failed
            .iter()
            .filter(|cr| !is_advisory_check_name(&cr.name))
            .copied()
            .collect(),
    }
}

/// Parse a GitHub Actions workflow-run id out of a check-run's `html_url`.
///
/// URLs look like `https://github.com/{owner}/{repo}/actions/runs/{run_id}/...`.
/// Returns `None` for URLs that don't carry a run id (e.g. non-Actions checks).
pub(crate) fn parse_actions_run_id(html_url: &str) -> Option<u64> {
    html_url
        .split("/actions/runs/")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .and_then(|s| s.parse::<u64>().ok())
}

/// True when a job/step conclusion represents a CI failure worth surfacing.
pub(crate) fn is_failing_conclusion(conclusion: Option<&str>) -> bool {
    matches!(
        conclusion,
        Some("failure") | Some("timed_out") | Some("cancelled")
    )
}

/// Build the human-readable `sections` and the structured `ci_jobs` payload for
/// a CI-failure rework comment from the *union* of failing jobs across one or
/// more workflow runs.
///
/// `jobs` is `Some` when the Actions API returned job data for at least one run
/// (possibly empty), and `None` when no run jobs could be fetched at all — in
/// which case we fall back to listing the raw failed check-run names. The output
/// format matches what the worker already expects (Workflow / Failed job /
/// Failed step lines plus `ci_job_log(...)` hint lines and a `ci_jobs` array);
/// it is just unioned across every failing run rather than the first one only.
///
/// `jobs` is expected to already be de-duplicated by job id by the caller.
pub(crate) fn build_ci_failure_sections(
    jobs: Option<&[ActionsJob]>,
    failed_checks: &[&CheckRun],
) -> (Vec<String>, Vec<serde_json::Value>) {
    let mut sections: Vec<String> = Vec::new();
    let mut ci_jobs: Vec<serde_json::Value> = Vec::new();

    match jobs {
        Some(jobs) => {
            // One `**Workflow:** <name>` header per distinct workflow, in first-seen
            // order, so multi-run aggregation names every workflow that failed.
            let mut seen_workflows: Vec<&str> = Vec::new();
            for name in jobs.iter().filter_map(|j| j.workflow_name.as_deref()) {
                if !seen_workflows.contains(&name) {
                    seen_workflows.push(name);
                    sections.push(format!("**Workflow:** {name}"));
                }
            }

            for job in jobs
                .iter()
                .filter(|j| is_failing_conclusion(j.conclusion.as_deref()))
            {
                let conclusion = job.conclusion.as_deref().unwrap_or("unknown");
                sections.push(format!("**Failed job:** {} ({})", job.name, conclusion));

                let failed_steps: Vec<_> = job
                    .steps
                    .iter()
                    .filter(|s| is_failing_conclusion(s.conclusion.as_deref()))
                    .collect();

                for step in &failed_steps {
                    let step_conclusion = step.conclusion.as_deref().unwrap_or("unknown");
                    sections.push(format!(
                        "**Failed step:** {} (step #{}, {})",
                        step.name, step.number, step_conclusion
                    ));
                }

                sections.push(format!("Job URL: {}", job.html_url));

                // Structured metadata for the `ci_job_log` tool. The worker can
                // call `ci_job_log(job_id=...)` to fetch the full log on demand.
                let failed_step_names: Vec<serde_json::Value> = failed_steps
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "name": s.name,
                            "number": s.number,
                        })
                    })
                    .collect();

                ci_jobs.push(serde_json::json!({
                    "job_id": job.id,
                    "name": job.name,
                    "failed_steps": failed_step_names,
                }));
            }
        }
        None => {
            // Fallback: just list the check run names.
            for cr in failed_checks {
                let conclusion = cr.conclusion.as_deref().unwrap_or("unknown");
                sections.push(format!(
                    "- **{}** ({}): {}",
                    cr.name, conclusion, cr.html_url
                ));
            }
        }
    }

    // Build `ci_job_log` hint lines so the worker knows exactly which tool call
    // to make for each failed job (across all aggregated runs).
    if !ci_jobs.is_empty() {
        let hints: Vec<String> = ci_jobs
            .iter()
            .map(|j| {
                let job_id = j["job_id"].as_u64().unwrap_or(0);
                let name = j["name"].as_str().unwrap_or("unknown");
                let steps: Vec<String> = j["failed_steps"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|s| s["name"].as_str().map(|n| n.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                if let Some(step) = steps.first() {
                    format!(
                        "Use `ci_job_log(job_id={job_id}, step=\"{step}\")` to view the **{name}** failed step log."
                    )
                } else {
                    format!("Use `ci_job_log(job_id={job_id})` to view the **{name}** job log.")
                }
            })
            .collect();
        sections.push(format!("\n{}", hints.join("\n")));
    }

    (sections, ci_jobs)
}

/// Render the informational section listing failed checks that are NOT part of
/// any required workflow run (advisory/preview integrations in their own
/// workflows — Vercel deploy previews, optional bot gates, etc.). These are not
/// keeping the *required* gate red, so a code diff cannot un-block the merge by
/// fixing them; they ride along purely as context for the worker spawned by a
/// legitimate reopen (a required check failed / reviewer requested changes).
///
/// IMPORTANT: this section must only ever list checks that are genuinely outside
/// the required gate's workflow run(s). The failing jobs *inside* a required
/// aggregate gate (e.g. `Server Clippy` under `Quality Gate`) are blockers — see
/// [`blocking_failed_checks`] — and must never be demoted into this section,
/// because that would tell the worker to ignore the very job keeping the
/// required check red.
pub(crate) fn advisory_checks_section(advisory_failed: &[&CheckRun]) -> Option<String> {
    if advisory_failed.is_empty() {
        return None;
    }
    let lines: Vec<String> = advisory_failed
        .iter()
        .map(|cr| {
            let conclusion = cr.conclusion.as_deref().unwrap_or("unknown");
            format!("- {} ({}): {}", cr.name, conclusion, cr.html_url)
        })
        .collect();
    Some(format!(
        "\n**Other failing checks outside the required gate (informational):**\n{}\n\
         _These checks run in their own workflows, separate from the required \
         merge gate, so fixing them will not by itself un-block the merge. They \
         are listed for context. The required checks above are what gate \
         merging — make those green._",
        lines.join("\n")
    ))
}

/// Compute a stable fingerprint from normalized failing-check names and CI
/// failure sections. The fingerprint is a hash of:
/// 1. Sorted, deduplicated failing check-run names (normalized: lowercase, trimmed)
/// 2. The workflow names + failed job names + failed step names from the CI
///    failure sections (the structured content, not the full text)
///
/// Returns a hex string. Two CI failures with the same fingerprint indicate
/// the worker is hitting the exact same checks/errors across pushes.
pub(crate) fn compute_ci_failure_fingerprint(
    failed_checks: &[&CheckRun],
    ci_failure_sections: &[String],
) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    // 1. Normalize check names
    let mut check_names: Vec<String> = failed_checks
        .iter()
        .map(|cr| cr.name.to_lowercase().trim().to_string())
        .collect();
    check_names.sort();
    check_names.dedup();

    // 2. Extract structured failure markers from sections
    //    Parse lines starting with "**Failed job:**" or "**Failed step:**"
    let mut failure_markers: Vec<String> = ci_failure_sections
        .iter()
        .filter_map(|s| {
            if s.starts_with("**Failed job:**") || s.starts_with("**Failed step:**") {
                Some(s.clone())
            } else {
                None
            }
        })
        .collect();
    failure_markers.sort();
    failure_markers.dedup();

    // 3. Hash combined content
    let combined = format!(
        "checks:{}|failures:{}",
        check_names.join(","),
        failure_markers.join(",")
    );
    let mut hasher = DefaultHasher::new();
    combined.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

/// Walk activity entries in reverse chronological order and count how many
/// consecutive entries have a fingerprint matching `current_fp`.
/// Stops at the first different fingerprint.
pub(crate) fn count_consecutive_identical(
    entries: &[djinn_core::models::ActivityEntry],
    current_fp: &str,
) -> u32 {
    let mut count = 0u32;
    for entry in entries.iter().rev() {
        let parsed: serde_json::Value = match serde_json::from_str(&entry.payload) {
            Ok(v) => v,
            Err(_) => break,
        };
        let fp = match parsed.get("fingerprint").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => break,
        };
        if fp == current_fp {
            count += 1;
        } else {
            break;
        }
    }
    count
}

/// Determine whether a CI failure is a scope-inversion: the failing crates/files
/// are OUTSIDE the PR's own git diff.
///
/// Returns:
/// - `Some(true)` — scope inversion detected (CI fails on crates outside the PR diff)
/// - `Some(false)` — CI fails on crates WITHIN the PR diff (normal worker bug)
/// - `None` — inconclusive (can't attribute failures to specific files/crates,
///   e.g. workspace-wide errors, no file path in failure sections)
pub(crate) fn detect_scope_inversion(
    ci_failure_sections: &[String],
    pr_files: &[String], // file paths from the PR diff
) -> Option<bool> {
    if pr_files.is_empty() {
        return None;
    }

    // 1. Extract failing crate names from CI failure sections.
    let failing_crates = extract_crate_names_from_sections(ci_failure_sections);
    if failing_crates.is_empty() {
        return None;
    }

    // 2. Extract crate names from PR diff files.
    let pr_crates = extract_crate_names(pr_files);
    if pr_crates.is_empty() {
        return None;
    }

    // 3. Compare the sets:
    //    - If ANY failing crate is NOT in the PR's crate set → Some(true)
    //    - If ALL failing crates ARE in the PR's crate set → Some(false)
    let pr_crate_set: std::collections::HashSet<&str> =
        pr_crates.iter().map(|s| s.as_str()).collect();
    let any_outside = failing_crates
        .iter()
        .any(|c| !pr_crate_set.contains(c.as_str()));

    if any_outside { Some(true) } else { Some(false) }
}

/// Extract crate names from a list of file paths using a simple heuristic:
/// - `server/crates/<crate-name>/src/...` → `<crate-name>`
/// - `crates/<crate-name>/src/...` → `<crate-name>`
/// - Paths without `crates/` return `None`.
pub(crate) fn extract_crate_name(path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('/').collect();
    for i in 0..parts.len().saturating_sub(1) {
        if parts[i] == "crates" {
            return parts.get(i + 1).map(|s| s.to_string());
        }
    }
    None
}

/// Extract crate names from a list of file paths, deduplicated and sorted.
pub(crate) fn extract_crate_names(paths: &[String]) -> Vec<String> {
    let mut crates: Vec<String> = paths.iter().filter_map(|p| extract_crate_name(p)).collect();
    crates.sort();
    crates.dedup();
    crates
}

/// Extract crate names from CI failure sections by looking for file paths
/// embedded in the failure text. We look for:
/// - Rust compiler error locations: `--> path/to/file.rs:line:col`
/// - File paths in "Failed step:" or "Failed job:" names that contain crate paths
/// - Any path segment containing `crates/`
pub(crate) fn extract_crate_names_from_sections(sections: &[String]) -> Vec<String> {
    let mut crates = std::collections::HashSet::new();
    for section in sections {
        // Look for `--> path/to/file.rs:line:col` pattern (Rust compiler errors)
        for line in section.lines() {
            if let Some(arrow_idx) = line.find("-->") {
                let after_arrow = &line[arrow_idx + 3..];
                let trimmed = after_arrow.trim();
                // Strip trailing `:line:col` if present
                let path_part = if let Some(colon_idx) = trimmed.rfind(':') {
                    let before_last_colon = &trimmed[..colon_idx];
                    if before_last_colon.rfind(':').is_some() {
                        // Likely `path:line:col` — extract the path part
                        if let Some(prev_colon) = before_last_colon.rfind(':') {
                            let candidate = &trimmed[..prev_colon];
                            // Verify candidate looks like a path (contains '/')
                            if candidate.contains('/') {
                                candidate
                            } else {
                                trimmed
                            }
                        } else {
                            trimmed
                        }
                    } else {
                        trimmed
                    }
                } else {
                    trimmed
                };
                if let Some(crate_name) = extract_crate_name(path_part) {
                    crates.insert(crate_name);
                }
            }
        }

        // Also look for any `crates/<name>/` pattern in the text
        if let Some(start) = section.find("crates/") {
            let after = &section[start + 7..];
            if let Some(end) = after.find('/') {
                let crate_name = &after[..end];
                if !crate_name.is_empty() {
                    crates.insert(crate_name.to_string());
                }
            }
        }
    }
    let mut result: Vec<String> = crates.into_iter().collect();
    result.sort();
    result
}
