// djinn:allow-oversize
use super::*;
use djinn_core::models::CiStatus;
use djinn_provider::github_api::RequiredCheckReproduction;

use super::ci_failure_analysis::{
    compute_ci_failure_fingerprint, detect_scope_inversion, extract_crate_names,
    extract_crate_names_from_sections,
};
use crate::ci_reproduction::{format_reproduction_plan, format_unreproducible_intervention_reason};

/// Timeout for fetching reproduction context from the GitHub API when building
/// a focused reproduction plan for same-signature escalation.
const REPRODUCTION_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

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
    /// Gates rework on five things (in order):
    /// 1. **Blocking-only filter** — intersect with required status checks.
    /// 2. **Same-CI-signature** — escalate after `SAME_CI_SIGNATURE_THRESHOLD`
    ///    identical fingerprints (content-aware, independent of `reopen_count`).
    /// 3. **Scope-inversion** — CI fails on crates outside the PR diff → RE-SLICE.
    /// 4. **Diff-empty** — head has no commits ahead of base → escalate + PARK.
    /// 5. **Cycle cap** — past `PR_CI_FAILURE_THRESHOLD` → escalate + PARK.
    ///
    /// Returns `true` when the event was consumed (task transitioned); `false`
    /// when failures were all non-blocking (caller falls through).
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
        //
        // The authoritative counter is the durable CI gate snapshot's
        // `same_signature_count` field — NOT activity-log entries. The
        // `record_ci_snapshot` path already calls `reset_ci_snapshot_for_head`
        // when the head SHA changes, which zeros the counter and clears the
        // fingerprint. If the fingerprint changed (different failures =
        // progress), we also restart the count here. This makes escalation
        // independent of `reopen_count`.
        //
        // m116: when the task carries mirror↔GitHub head divergence evidence
        // (`ci_heads_diverged == Some(true)`) or a recent publication
        // observation error, the same-GitHub-head observation is NOT a
        // worker-stuck signal — the worker's commit never reached GitHub, so
        // GitHub just keeps re-evaluating the same old failing head. Treat the
        // observation as a publication-failure evidence, do NOT carry forward
        // the prior same-signature count, do NOT bump the counter, and emit a
        // divergence-aware activity event so operators investigate re-push
        // instead of seeing a misleading same-GitHub-head escalation.
        let fingerprint = compute_ci_failure_fingerprint(&blocking, &ci_failure_sections);

        let task_repo = self.task_repo();
        let (prior_same_sig_count, same_signature_strike_suppressed) = match task_repo
            .get_ci_snapshot_for_task_pr(task_id, pull_number as i64)
            .await
        {
            Ok(Some(snap)) => {
                let divergence_observed = task.ci_heads_diverged == Some(true);
                let publication_error_observed = task.ci_head_observation_error.is_some();
                let same_fingerprint_as_persisted =
                    snap.failure_fingerprint.as_deref() == Some(&fingerprint);

                if divergence_observed || publication_error_observed {
                    // Reset the counter to zero and clear the fingerprint so
                    // a future legit worker-stuck count starts from scratch.
                    // Do NOT increment to a strike — this observation is a
                    // publication-failure false-positive.
                    tracing::warn!(
                        task_id = %task_short_id,
                        pr = pull_number,
                        sha = %current_sha,
                        heads_diverged = ?task.ci_heads_diverged,
                        publication_error = ?task.ci_head_observation_error,
                        mirror_head = ?task.ci_mirror_head_sha.as_deref(),
                        github_head = ?task.ci_github_head_sha.as_deref(),
                        "PR poller: same-GitHub-head observation suppressed — mirror↔GitHub \
                         publication divergence recorded; not counting this as a same-signature strike"
                    );
                    (0i64, true)
                } else if same_fingerprint_as_persisted {
                    (snap.same_signature_count, false)
                } else {
                    (0i64, false)
                }
            }
            Ok(None) => (0i64, false),
            Err(e) => {
                tracing::warn!(
                    task_id = %task_short_id,
                    error = %e,
                    "PR poller: failed to read CI snapshot for same-signature count; assuming 0"
                );
                (0i64, false)
            }
        };
        // The current failure is the (prior + 1)th identical fingerprint,
        // EXCEPT when m116 publication-failure evidence suppresses the
        // strike — in that case we hold the counter at zero and let the
        // next real worker round (after re-publish) restart the count.
        let total_consecutive = if same_signature_strike_suppressed {
            0
        } else {
            prior_same_sig_count + 1
        };

        // Persist the failing CI snapshot before checking escalation thresholds.
        // Set last_remediation_base_sha to the current failing head so later
        // submit handling can compare against that baseline.
        let blocking_names: Vec<String> = blocking.iter().map(|cr| cr.name.clone()).collect();
        self.persist_ci_snapshot(
            task_id,
            pull_number,
            current_sha,
            CiStatus::Failing,
            blocking_names,
            Some(fingerprint.clone()),
            total_consecutive,
            Some(current_sha.to_owned()),
        )
        .await;

        // ── Emit divergence-aware activity event when strike is suppressed ────
        // Operators see this alongside the structured CI snapshot so they
        // understand why an otherwise same-GitHub-head observation did not
        // produce a same-signature strike. The payload surfaces the same
        // reconciliation fields used by the supervisor path so audit trails
        // correlate supervisor-side and pr_poller-side events.
        if same_signature_strike_suppressed {
            let payload = serde_json::json!({
                "task_id": task_id,
                "short_id": task_short_id,
                "pr_number": pull_number,
                "current_sha": current_sha,
                "fingerprint": fingerprint,
                "mirror_head_sha": task.ci_mirror_head_sha,
                "github_head_sha": task.ci_github_head_sha,
                "heads_diverged": task.ci_heads_diverged,
                "head_observation_error": task.ci_head_observation_error,
                "reason": "same-GitHub-head CI failure suppressed — mirror↔GitHub publication divergence; no same-signature strike counted",
            })
            .to_string();
            if let Err(e) = task_repo
                .log_activity(
                    Some(task_id),
                    "coordinator",
                    "system",
                    "same_signature_strike_suppressed_unpublished_mirror",
                    &payload,
                )
                .await
            {
                tracing::warn!(
                    task_id = %task_short_id,
                    pr = pull_number,
                    error = %e,
                    "PR poller: failed to log same-signature strike suppression activity"
                );
            }
        }

        if total_consecutive >= SAME_CI_SIGNATURE_THRESHOLD as i64 {
            let blocking_names: Vec<&str> = blocking.iter().map(|cr| cr.name.as_str()).collect();

            // ── Build a focused reproduction plan from the provider bundle ──
            // Fetch the CI-introspection bundle for each blocking check.
            // Reproducible bundles yield a reproduction plan; unreproducible
            // bundles yield a typed intervention reason.  If every fetch errors
            // (API down, timeout), fall back to the original generic reason.
            let mut reproducible_ctxs: Vec<
                djinn_provider::github_api::RequiredCheckReproductionContext,
            > = Vec::new();
            let mut unreproducible_reasons: Vec<String> = Vec::new();
            let mut any_fetch_succeeded = false;

            for check in &blocking {
                let fetch = tokio::time::timeout(
                    REPRODUCTION_FETCH_TIMEOUT,
                    gh_client.required_check_reproduction_context(
                        owner,
                        repo,
                        current_sha,
                        &check.name,
                    ),
                )
                .await;
                match fetch {
                    Ok(Ok(RequiredCheckReproduction::Reproducible(ctx))) => {
                        any_fetch_succeeded = true;
                        reproducible_ctxs.push(ctx);
                    }
                    Ok(Ok(RequiredCheckReproduction::Unreproducible(u))) => {
                        any_fetch_succeeded = true;
                        unreproducible_reasons.push(format_unreproducible_intervention_reason(&u));
                    }
                    Ok(Err(e)) => {
                        tracing::info!(
                            task_id = %task_short_id,
                            check = %check.name,
                            error = %e,
                            "PR poller: failed to fetch reproduction context for same-signature check"
                        );
                    }
                    Err(_) => {
                        tracing::info!(
                            task_id = %task_short_id,
                            check = %check.name,
                            "PR poller: timeout fetching reproduction context for same-signature check"
                        );
                    }
                }
            }

            let repro_ctx = SameSignatureReproContext {
                reproducible_ctxs,
                unreproducible_reasons,
                any_fetch_succeeded,
            };

            match classify_same_signature_escalation(&repro_ctx) {
                SameSignatureEscalationRoute::GenericIntervention => {
                    // Every fetch errored (API down, timeout): fall back to the
                    // original generic escalation so the same-signature path is
                    // never silently skipped.
                    let reason = build_generic_same_signature_reason(
                        pull_number,
                        total_consecutive,
                        &blocking_names,
                    );
                    tracing::warn!(
                        task_id = %task_short_id,
                        pr = pull_number,
                        fingerprint = %fingerprint,
                        consecutive = total_consecutive,
                        threshold = SAME_CI_SIGNATURE_THRESHOLD,
                        "PR poller: same CI failure signature — \
                         all reproduction-context fetches failed; escalating generically"
                    );
                    let sections_text = ci_failure_sections.join("\n");
                    self.route_planner_intervention(
                        task,
                        "worker",
                        &reason,
                        Some(&sections_text),
                        task.reopen_count,
                    )
                    .await;
                }
                SameSignatureEscalationRoute::UnreproducibleIntervention => {
                    // Every bundle is unreproducible: route to human/lead
                    // intervention with the distinct unreproducible reason.
                    let reason = build_unreproducible_same_signature_reason(
                        pull_number,
                        total_consecutive,
                        &blocking_names,
                        &repro_ctx.unreproducible_reasons,
                    );
                    tracing::warn!(
                        task_id = %task_short_id,
                        pr = pull_number,
                        fingerprint = %fingerprint,
                        consecutive = total_consecutive,
                        threshold = SAME_CI_SIGNATURE_THRESHOLD,
                        unreproducible_count = repro_ctx.unreproducible_reasons.len(),
                        "PR poller: same CI failure signature — all bundles unreproducible; \
                         routing to human intervention"
                    );
                    // Create a HumanReview remediation task with the
                    // unreproducible reason.  No Planner dispatch — a human
                    // must resolve it.
                    self.escalate_ci_failure_and_park(task, pr_url, &reason, &ci_failure_sections)
                        .await;
                }
                SameSignatureEscalationRoute::ReproductionPlan => {
                    // At least one reproducible bundle: build a focused
                    // reproduction plan for the remediation payload.
                    let reason = build_reproduction_plan_same_signature_reason(
                        pull_number,
                        total_consecutive,
                        &blocking_names,
                        &repro_ctx.reproducible_ctxs,
                        &repro_ctx.unreproducible_reasons,
                    );
                    tracing::warn!(
                        task_id = %task_short_id,
                        pr = pull_number,
                        fingerprint = %fingerprint,
                        consecutive = total_consecutive,
                        threshold = SAME_CI_SIGNATURE_THRESHOLD,
                        reproducible_count = repro_ctx.reproducible_ctxs.len(),
                        unreproducible_count = repro_ctx.unreproducible_reasons.len(),
                        "PR poller: same CI failure signature — emitting focused reproduction plan"
                    );
                    // Create a HumanReview remediation task with the reproduction
                    // plan as the reason.  This parks the source on the blocker
                    // without dispatching a Planner (no scope reshape for
                    // ordinary reproduced CI loops).
                    self.escalate_ci_failure_and_park(task, pr_url, &reason, &ci_failure_sections)
                        .await;
                }
            }
            return true;
        }

        // Record this fingerprint marker as an audit trail. Since sa4x the
        // durable snapshot's `same_signature_count` is the authoritative
        // counter; the activity-log entry is retained for operator visibility
        // only.
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
            self.route_planner_intervention(
                task,
                "worker",
                &reason,
                Some(&sections_text),
                task.reopen_count,
            )
            .await;
            self.terminalize_for_pr_outcome(
                task_id,
                djinn_core::models::task_attempt::TaskAttemptOutcome::Reopened,
                Some(&TransitionAction::PrCiFailed),
                Some(&reason),
            )
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

        self.terminalize_for_pr_outcome(
            &task.id,
            djinn_core::models::task_attempt::TaskAttemptOutcome::Reopened,
            Some(&TransitionAction::PrCiFailed),
            Some(reason),
        )
        .await;

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

        // Route the CI-loop park through the autonomous escalation ladder: a
        // planner-park escalation blocks + parks the source (the Planner owns
        // terminal resolution), OR — once the autonomous-escalation ceiling is
        // spent on this source — the source is terminally failed rather than
        // held for a person. NO human-review hold is produced on this path.
        let enriched_reason = if ci_failure_sections.is_empty() {
            reason.to_string()
        } else {
            format!(
                "{reason}\n\n**CI Failure Details:**\n{}",
                ci_failure_sections.join("\n")
            )
        };
        // `escalate_to_planner_or_terminally_fail` adds the blocker BEFORE
        // parking the source, so the open task is never dispatchable without its
        // blocker; when the ceiling is reached it ForceCloses the source instead
        // (which releases its blockers) and does NOT park.
        self.escalate_to_planner_or_terminally_fail(task, &enriched_reason)
            .await;
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

    /// Resolve the merge methods this repository actually permits, in Djinn's
    /// preference order (squash → merge commit → rebase). The first entry is
    /// the method to attempt first; the rest are fallbacks.
    ///
    /// Reads `GET /repos` (`allow_squash_merge` / `allow_merge_commit` /
    /// `allow_rebase_merge`). On a fetch failure we degrade to trying all three
    /// methods by trial in preference order, so the merge attempt loop can still
    /// discover a permitted method even when the config read was transiently
    /// unavailable — never falling back to a squash-only assumption that would
    /// re-wedge a squash-disabled repo.
    pub(crate) async fn resolve_allowed_merge_methods(
        &self,
        gh_client: &GitHubApiClient,
        owner: &str,
        repo: &str,
    ) -> Vec<MergeMethod> {
        match gh_client.get_repo_merge_config(owner, repo).await {
            Ok(cfg) => allowed_merge_methods(&cfg),
            Err(e) => {
                tracing::info!(
                    owner,
                    repo,
                    error = %e,
                    "PR poller: could not read repo merge config — trying squash→merge→rebase by trial"
                );
                vec![MergeMethod::Squash, MergeMethod::Merge, MergeMethod::Rebase]
            }
        }
    }

    /// Attempt to merge a PR, trying each allowed method in preference order and
    /// falling back ONLY when GitHub rejects the attempted method as "not
    /// allowed on this repository" (see [`is_merge_method_not_allowed`]).
    ///
    /// Any other error — the merge-queue 405, a conversation-resolution block, a
    /// transient/5xx, an approval-required rejection — is returned immediately so
    /// the caller's existing specialised handlers deal with it. When every
    /// allowed method is rejected as not-allowed, the last such error is returned
    /// (which the caller detects with `is_merge_method_not_allowed` and routes to
    /// escalation instead of an infinite retry loop).
    pub(crate) async fn merge_pr_with_fallback(
        &self,
        gh_client: &GitHubApiClient,
        owner: &str,
        repo: &str,
        pull_number: u64,
        commit_title: &str,
        methods: &[MergeMethod],
    ) -> std::result::Result<serde_json::Value, anyhow::Error> {
        let mut last_not_allowed: Option<anyhow::Error> = None;
        for (idx, method) in methods.iter().enumerate() {
            match gh_client
                .merge_pull_request(owner, repo, pull_number, *method, commit_title)
                .await
            {
                Ok(v) => return Ok(v),
                Err(e) => {
                    let e: anyhow::Error = e.into();
                    if is_merge_method_not_allowed(&e) {
                        tracing::info!(
                            owner,
                            repo,
                            pr = pull_number,
                            method = ?method,
                            fallbacks_remaining = methods.len().saturating_sub(idx + 1),
                            "PR poller: merge method rejected as not allowed — trying next allowed method"
                        );
                        last_not_allowed = Some(e);
                        continue;
                    }
                    // Non-method error (merge queue, conversation block,
                    // transient) — surface immediately for the caller's
                    // specialised handling.
                    return Err(e);
                }
            }
        }
        Err(last_not_allowed
            .unwrap_or_else(|| anyhow::anyhow!("no merge methods available to attempt")))
    }
}
pub(crate) fn is_merge_queue_405(
    err: &(impl crate::github_error_render::GithubWriteError + ?Sized),
) -> bool {
    crate::github_error_render::github_write_status_is(err, 405)
        && crate::github_error_render::github_write_body_contains(err, "merge queue")
}

/// Detect GitHub's "this merge method is disabled on the repository" rejection.
///
/// The PUT `/merge` endpoint returns a 405 (occasionally 422) whose body reads
/// "Squash merges are not allowed on this repository." (likewise "Merge commits
/// are not allowed…", "Rebase merges are not allowed…") when the chosen
/// [`MergeMethod`] is disabled in the repo's settings. This is distinct from the
/// merge-queue 405 (body contains "merge queue", checked first) and the
/// conversation-resolution block — retrying the same method can never succeed,
/// so callers fall back to another allowed method and ultimately escalate.
pub(crate) fn is_merge_method_not_allowed(
    err: &(impl crate::github_error_render::GithubWriteError + ?Sized),
) -> bool {
    // Never confuse the merge-queue 405 (a delegated-to-GitHub signal) for a
    // disallowed merge method.
    if is_merge_queue_405(err) {
        return false;
    }
    let status_matches = crate::github_error_render::github_write_status_is(err, 405)
        || crate::github_error_render::github_write_status_is(err, 422);
    status_matches && crate::github_error_render::github_write_body_contains(err, "are not allowed")
}

/// The repository's allowed merge methods in Djinn's preference order:
/// squash → merge commit → rebase. The first element is the method Djinn will
/// attempt first; the remainder are fallbacks used when GitHub rejects the
/// chosen method as "not allowed" (a stale-cache / race guard).
///
/// Never returns empty: a pathological config with every method disabled still
/// yields `[Squash]` so the caller makes one attempt and then escalates via the
/// method-not-allowed path rather than silently doing nothing.
pub(crate) fn allowed_merge_methods(cfg: &RepoMergeConfig) -> Vec<MergeMethod> {
    let mut methods = Vec::with_capacity(3);
    if cfg.allow_squash_merge {
        methods.push(MergeMethod::Squash);
    }
    if cfg.allow_merge_commit {
        methods.push(MergeMethod::Merge);
    }
    if cfg.allow_rebase_merge {
        methods.push(MergeMethod::Rebase);
    }
    if methods.is_empty() {
        methods.push(MergeMethod::Squash);
    }
    methods
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

/// True when a conclusion is a *hard* failure — the work itself broke, as
/// opposed to `cancelled`, which under fail-fast semantics (GitHub's native
/// `fail-fast: true` matrices, or jobs that cancel the run on first failure)
/// usually just means a sibling job failed first.
pub(crate) fn is_hard_failure_conclusion(conclusion: Option<&str>) -> bool {
    matches!(conclusion, Some("failure") | Some("timed_out"))
}

/// True when the job — or any of its steps — hard-failed. Step-level
/// detection matters under fail-fast: a job that cancels its own run after a
/// step fails can end up `cancelled` at the job level (the cancel signal
/// races its post steps), while the failed step's conclusion is sealed first.
pub(crate) fn job_hard_failed(job: &ActionsJob) -> bool {
    is_hard_failure_conclusion(job.conclusion.as_deref())
        || job
            .steps
            .iter()
            .any(|step| is_hard_failure_conclusion(step.conclusion.as_deref()))
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

            // Fail-fast cancellation (GitHub's `fail-fast: true` matrices, or
            // jobs cancelling the run on first failure) marks every sibling
            // job `cancelled` alongside the one real failure. Those cancelled
            // jobs are fallout, not signal — listing them sends the worker
            // chasing logs of jobs that never finished. Only surface
            // cancelled jobs when nothing hard-failed.
            let has_hard_failure = jobs.iter().any(job_hard_failed);

            for job in jobs.iter().filter(|j| {
                if has_hard_failure {
                    job_hard_failed(j)
                } else {
                    is_failing_conclusion(j.conclusion.as_deref())
                }
            }) {
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

// CI failure fingerprinting and scope attribution live in `ci_failure_analysis`
// to keep this poller helper below the changed-file size guard.

// ── Same-signature reproduction-plan escalation reason builders ──────────────
//
// Pure functions that build the escalation reason text for the three branches
// of the same-signature escalation path.  Extracted from the inline logic in
// `handle_ci_failure` so regression tests can verify the full plan/reason
// contract without a live DB or GitHub client.

/// The outcome of collecting reproduction context for all blocking checks at
/// same-signature escalation time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SameSignatureReproContext {
    /// Reproducible bundles collected from the provider.
    pub reproducible_ctxs: Vec<djinn_provider::github_api::RequiredCheckReproductionContext>,
    /// Formatted unreproducible reasons (one per unreproducible bundle).
    pub unreproducible_reasons: Vec<String>,
    /// True when at least one provider fetch succeeded (even if unreproducible).
    pub any_fetch_succeeded: bool,
}

/// Classify the same-signature escalation into one of three routing strategies
/// based on the collected reproduction context.  This mirrors the branching in
/// `handle_ci_failure` and makes the decision testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SameSignatureEscalationRoute {
    /// Every fetch errored/timed out — fall back to generic planner intervention.
    GenericIntervention,
    /// All fetched bundles are unreproducible — route to human intervention
    /// with the concrete unreproducible reasons.
    UnreproducibleIntervention,
    /// At least one bundle is reproducible — emit a focused reproduction plan.
    ReproductionPlan,
}

/// Determine which escalation route applies for the given collected context.
pub(crate) fn classify_same_signature_escalation(
    ctx: &SameSignatureReproContext,
) -> SameSignatureEscalationRoute {
    if !ctx.any_fetch_succeeded {
        SameSignatureEscalationRoute::GenericIntervention
    } else if ctx.reproducible_ctxs.is_empty() {
        SameSignatureEscalationRoute::UnreproducibleIntervention
    } else {
        SameSignatureEscalationRoute::ReproductionPlan
    }
}

/// Build the generic escalation reason when all reproduction-context fetches
/// fail (API down, timeout).  This is the fallback that preserves the original
/// same-signature behaviour so the path is never silently skipped.
pub(crate) fn build_generic_same_signature_reason(
    pull_number: u64,
    total_consecutive: i64,
    blocking_names: &[&str],
) -> String {
    format!(
        "PR #{pull_number}: CI has failed with the identical fingerprint \
         {total_consecutive} consecutive times (checks: {checks}). \
         The worker is not making progress on these specific failures. \
         Escalating for planner intervention.",
        checks = blocking_names.join(", "),
    )
}

/// Build the human-intervention reason when every fetched bundle is
/// unreproducible.  The reason lists each distinct unreproducible reason so a
/// human/lead knows exactly why the checks cannot be reproduced locally.
pub(crate) fn build_unreproducible_same_signature_reason(
    pull_number: u64,
    total_consecutive: i64,
    blocking_names: &[&str],
    unreproducible_reasons: &[String],
) -> String {
    let mut reason = format!(
        "PR #{pull_number}: CI has failed with the identical fingerprint \
         {total_consecutive} consecutive times (checks: {checks}). \
         No required check could be reproduced locally:",
        checks = blocking_names.join(", "),
    );
    for ur in unreproducible_reasons {
        reason.push_str(&format!("\n- {ur}"));
    }
    reason
}

/// Build the focused reproduction-plan reason when at least one blocking check
/// is reproducible.  The plan includes the failing check, job/step, derived
/// command/setup, CI log tail, observed head SHA, and reproduce → fix → verify
/// → resubmit instructions.  Unreproducible checks (if any) are appended.
pub(crate) fn build_reproduction_plan_same_signature_reason(
    pull_number: u64,
    total_consecutive: i64,
    blocking_names: &[&str],
    reproducible_ctxs: &[djinn_provider::github_api::RequiredCheckReproductionContext],
    unreproducible_reasons: &[String],
) -> String {
    let plan_sections: Vec<String> = reproducible_ctxs
        .iter()
        .map(format_reproduction_plan)
        .collect();
    let mut reason = format!(
        "PR #{pull_number}: CI has failed with the identical fingerprint \
         {total_consecutive} consecutive times (checks: {checks}). \
         Focused remediation plan:\n\n{plan}",
        checks = blocking_names.join(", "),
        plan = plan_sections.join("\n\n"),
    );
    if !unreproducible_reasons.is_empty() {
        reason.push_str("\n\n**Unreproducible checks:**\n");
        for ur in unreproducible_reasons {
            reason.push_str(&format!("- {ur}\n"));
        }
    }
    reason
}

// ── CI merge gate ────────────────────────────────────────────────────────────

/// Verdict of the CI merge gate — determines whether Djinn may initiate a
/// merge or close action on a PR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CiMergeGateVerdict {
    /// CI is passing on the current head — merge/close is allowed.
    Allow,
    /// CI state is not yet determined (pending/unknown) or the snapshot is
    /// stale/missing — hold without escalation; the next poller tick will
    /// re-evaluate.
    Hold,
    /// Required CI is actively failing on the current head — block merge.
    /// Remediation/intervention behavior handles this case.
    Block,
}

/// Check whether the durable CI snapshot allows Djinn to initiate a merge or
/// close action.
///
/// Returns [`CiMergeGateVerdict::Allow`] only when **all** of:
/// - A CI snapshot exists for the task/PR
/// - The snapshot's `head_sha` matches `current_head_sha` (no stale data)
/// - The snapshot's `ci_status` is `Passing`
///
/// | Snapshot state     | Verdict |
/// |--------------------|---------|
/// | `Passing` + fresh  | Allow   |
/// | `Pending`          | Hold    |
/// | `Unknown`          | Hold    |
/// | `Failing`          | Block   |
/// | Missing snapshot   | Hold    |
/// | Stale SHA          | Hold    |
///
/// This function is pure and does not access the database — callers read the
/// snapshot and pass it in.
pub(crate) fn ci_merge_gate_verdict(
    snapshot: Option<&djinn_core::models::TaskPrCiSnapshot>,
    current_head_sha: &str,
) -> CiMergeGateVerdict {
    match snapshot {
        None => CiMergeGateVerdict::Hold,
        Some(snap) => {
            if snap.head_sha != current_head_sha {
                return CiMergeGateVerdict::Hold;
            }
            match snap.ci_status {
                CiStatus::Passing => CiMergeGateVerdict::Allow,
                CiStatus::Pending | CiStatus::Unknown => CiMergeGateVerdict::Hold,
                CiStatus::Failing => CiMergeGateVerdict::Block,
            }
        }
    }
}
