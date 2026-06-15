use super::*;

impl CoordinatorActor {
    pub(in crate::actors::coordinator) async fn resolve_required_contexts(
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
    /// Fixes the infinite CI-failure rework loop by gating the rework on three
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
    /// 2. **Diff-empty short-circuit.** If the PR head has no commits ahead of
    ///    base on GitHub (`ahead_by == 0`), the previous worker iteration
    ///    produced no new diff — re-dispatching cannot change anything. We
    ///    escalate to the Planner and force-close instead of looping.
    ///
    /// 3. **Cycle cap.** Each CI-failure rework records a `pr_ci_cycle` marker.
    ///    Past `PR_CI_FAILURE_THRESHOLD` we escalate to the Planner and
    ///    force-close rather than redispatch. Escalation is terminal — the
    ///    counter is never reset on the reopen that re-arms the loop.
    ///
    /// Returns `true` when the event was *consumed* (the task was transitioned
    /// — either reworked or force-closed) and the caller should run its
    /// post-transition cache cleanup and `continue`. Returns `false` when the
    /// failures were all non-blocking and the caller should fall through to its
    /// normal (CI-passed) handling.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::actors::coordinator) async fn handle_ci_failure(
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

        // ── 2. Diff-empty short-circuit ───────────────────────────────────────
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
                    "PR poller: CI failed but branch is diff-empty vs base — escalating + force-closing"
                );
                self.escalate_ci_failure_and_close(task, pr_url, &reason)
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

        // ── 3. Cycle cap ──────────────────────────────────────────────────────
        let task_repo = self.task_repo();
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
                "PR poller: CI-failure rework threshold exceeded — escalating + force-closing"
            );
            self.escalate_ci_failure_and_close(task, pr_url, &reason)
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

    /// Terminal escalation for a CI-failure loop the worker can't resolve
    /// (diff-empty re-emit, or cycle cap exceeded). Logs a visibility comment,
    /// dispatches a Planner escalation, then `ForceClose`s the task so it
    /// leaves the rework loop. Never resets the CI-cycle counter.
    pub(in crate::actors::coordinator) async fn escalate_ci_failure_and_close(
        &mut self,
        task: &djinn_core::models::Task,
        pr_url: &str,
        reason: &str,
    ) {
        let task_repo = self.task_repo();

        let comment_body = format!("**PR CI Escalation**: {reason}\n\nPR: {pr_url}");
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

        // Escalate to the Planner (ADR-051 §8 escalation ceiling) before
        // force-closing, so a human / Planner sees why the task gave up.
        self.dispatch_planner_escalation(&task.id, reason, &task.project_id)
            .await;

        self.apply_pr_transition(&task.id, TransitionAction::ForceClose, Some(reason))
            .await;
        self.pr_status_cache.remove(&task.id);
        self.pr_draft_first_seen.remove(&task.id);
        self.merge_fail_count.remove(&task.id);
        self.delegated_to_github.remove(&task.id);
        self.conversations_resolved.remove(&task.id);
    }
}
pub(in crate::actors::coordinator) fn is_merge_queue_405(
    err: &(impl crate::github_error_render::GithubWriteError + ?Sized),
) -> bool {
    crate::github_error_render::github_write_status_is(err, 405)
        && crate::github_error_render::github_write_body_contains(err, "merge queue")
}

/// Detect the `enqueuePullRequest` UNPROCESSABLE rejection whose message is
/// "Pull request is already in the queue" — the entry we wanted already
/// exists (GitHub merge-when-ready armed it, or a pre-restart delegation this
/// process no longer remembers). Callers adopt the entry instead of erroring.
pub(in crate::actors::coordinator) fn is_already_queued(
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
pub(in crate::actors::coordinator) fn is_advisory_check_name(name: &str) -> bool {
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
///   checks from branch protection — the source of truth), keep only failed
///   checks whose name matches a required context. Anything not required is
///   advisory and must not trigger a rework.
/// - When `required_contexts` is `None` (branch protection unreadable), fall
///   back to the name-pattern heuristic: keep failed checks that are *not*
///   recognised as advisory. This is intentionally conservative — an unknown
///   check is treated as blocking so we never silently swallow a real failure.
pub(in crate::actors::coordinator) fn blocking_failed_checks<'a>(
    failed: &[&'a CheckRun],
    required_contexts: Option<&[String]>,
) -> Vec<&'a CheckRun> {
    match required_contexts {
        Some(required) => failed
            .iter()
            .filter(|cr| required.iter().any(|ctx| ctx == &cr.name))
            .copied()
            .collect(),
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
pub(in crate::actors::coordinator) fn parse_actions_run_id(html_url: &str) -> Option<u64> {
    html_url
        .split("/actions/runs/")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .and_then(|s| s.parse::<u64>().ok())
}

/// True when a job/step conclusion represents a CI failure worth surfacing.
pub(in crate::actors::coordinator) fn is_failing_conclusion(conclusion: Option<&str>) -> bool {
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
pub(in crate::actors::coordinator) fn build_ci_failure_sections(
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

/// Render the informational section listing failed checks that are NOT
/// required (advisory/preview/optional bot gates). These never drive a rework
/// on their own — the section exists so the worker spawned for a *legitimate*
/// reopen (required check failed / reviewer requested changes) knows about
/// them without treating them as blockers to loop on.
pub(in crate::actors::coordinator) fn advisory_checks_section(
    advisory_failed: &[&CheckRun],
) -> Option<String> {
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
        "\n**Non-required checks also failing (informational):**\n{}\n\
         _These checks do not gate merging and did not trigger this rework. \
         Do not loop on them — only address one if your change is clearly its \
         cause._",
        lines.join("\n")
    ))
}
