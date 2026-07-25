//! Durable CI snapshot recording — the sole writer of GitHub-derived CI gate
//! fields for a task's PR head.
//!
//! Split out of `pr_watcher` so the polling loop and the snapshot /
//! classification logic stay independently readable (and each within the
//! file-size guard).
//!
//! Classification is three-way, not two-way: a completed run whose blocking
//! required checks were *all* cancelled or never executed reached no verdict
//! about the code and is recorded as `CiStatus::Inconclusive`. See the
//! [`super::ci_triage`] module for the structural ranking rules that decide
//! which lane, if any, actually carries causal information.

use super::*;
use djinn_core::models::{CiStatus, TaskPrCiSnapshotInput};

/// Timeout for the best-effort annotation fetch on the primary blocking check.
/// Annotation capture is evidence, never a gate — a slow GitHub must not stall
/// the poll loop, so this is short and every failure path degrades to `None`.
const ANNOTATION_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

impl CoordinatorActor {
    /// Persist a CI gate snapshot. Write-through; failures are non-blocking.
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
            // This helper serves the no-CI / advisory-only fast paths, which by
            // construction have no causal blocking check and hence no evidence
            // to capture. `record_ci_snapshot` is the path that ranks.
            primary_blocking_check: None,
            failure_annotations: None,
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

    /// Fetch and render the annotations of the primary blocking check.
    ///
    /// Best-effort and size-bounded: annotation capture is evidence, never a
    /// gate. A failure to read annotations must not change the CI verdict, so
    /// every error path returns `None` after logging.
    async fn fetch_bounded_annotations(
        &self,
        gh_client: &GitHubApiClient,
        owner: &str,
        repo: &str,
        check_run_id: u64,
        check_name: &str,
        task_short_id: &str,
    ) -> Option<String> {
        match tokio::time::timeout(
            ANNOTATION_FETCH_TIMEOUT,
            gh_client.get_check_run_annotations(owner, repo, check_run_id),
        )
        .await
        {
            Ok(Ok(annotations)) => ci_triage::render_annotations(check_name, &annotations),
            Ok(Err(e)) => {
                tracing::debug!(
                    task_id = %task_short_id,
                    check = %check_name,
                    error = %e,
                    "PR poller: could not read check-run annotations (non-fatal)"
                );
                None
            }
            Err(_) => {
                tracing::debug!(
                    task_id = %task_short_id,
                    check = %check_name,
                    "PR poller: timed out reading check-run annotations (non-fatal)"
                );
                None
            }
        }
    }

    // ── CI snapshot recording (sole writer for GitHub-derived CI fields) ─────

    /// Record CI snapshot. Sole writer of GitHub-derived CI fields.
    /// Resets to `pending` when the head SHA changes.
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
        //
        // A completed run with blocking failures splits three ways, not two:
        //   * at least one blocking lane executed and hard-failed → `failing`
        //   * blocking lanes exist but every one was cancelled or never
        //     executed → `inconclusive` (a run-level abort reached no verdict
        //     about the code; retrigger, do not remediate)
        //   * no blocking lanes → `passing`
        let mut blocking_names: Vec<String> = Vec::new();
        let mut primary_check_name: Option<String> = None;
        let mut primary_check_id: Option<u64> = None;
        let mut failure_fingerprint: Option<String> = None;

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
                .filter(|cr| ci_helpers::is_failing_conclusion(cr.conclusion.as_deref()))
                .collect();

            let required_contexts = self
                .resolve_required_contexts(gh_client, owner, repo, base_ref, pull_number)
                .await;
            let blocking = blocking_failed_checks(&failed_checks, required_contexts.as_deref());

            if blocking.is_empty() {
                // All required checks passed (or no CI configured).
                CiStatus::Passing
            } else {
                // Persist the cascade in causal order so a reader triages it
                // top-down, with never-executed aggregators last.
                let ranked = ci_triage::rank_blocking_checks(&blocking);
                blocking_names = ranked.iter().map(|cr| cr.name.clone()).collect();

                match ci_triage::primary_blocking_check(&blocking) {
                    Some(primary) => {
                        primary_check_name = Some(primary.name.clone());
                        primary_check_id = Some(primary.id);

                        // Fingerprint from the CAUSAL lanes only. Cancelled
                        // siblings and never-executed aggregators carry
                        // incident-independent messages, so including them
                        // collapses distinct failures into one signature and
                        // makes `same_signature_count` count noise.
                        let causal = ci_triage::causal_checks(&blocking);
                        let (sections, _) = ci_helpers::build_ci_failure_sections(None, &causal);
                        failure_fingerprint =
                            Some(compute_ci_failure_fingerprint(&causal, &sections));
                        CiStatus::Failing
                    }
                    None => {
                        // Every blocking lane was cancelled or never executed.
                        tracing::info!(
                            task_id = %task_short_id,
                            pr = pull_number,
                            blocking_count = blocking.len(),
                            blocking = ?blocking_names,
                            "PR poller: all blocking required checks were cancelled or never \
                             executed — run is INCONCLUSIVE (no verdict about the code); \
                             retrigger rather than remediate"
                        );
                        CiStatus::Inconclusive
                    }
                }
            }
        } else {
            // Checks still running.
            CiStatus::Pending
        };

        // ── Capture the evidence that actually diagnoses the failure ────────
        //
        // Runner-host failures (out of disk, runner crash) surface ONLY as
        // check-run annotations — not as a conclusion, and often not in job
        // logs. Without this the board can name a check but never say why.
        let failure_annotations = match (primary_check_name.as_deref(), primary_check_id) {
            (Some(name), Some(id)) => {
                self.fetch_bounded_annotations(gh_client, owner, repo, id, name, task_short_id)
                    .await
            }
            _ => None,
        };

        let input = TaskPrCiSnapshotInput {
            task_id: task_id.to_string(),
            pr_number,
            head_sha: head_sha.to_string(),
            ci_status,
            blocking_required_check_names: blocking_names,
            primary_blocking_check: primary_check_name,
            failure_annotations,
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

    /// Record `unknown` CI status when GitHub data is unavailable.
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
                primary_blocking_check: None,
                failure_annotations: None,
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

    /// Re-run the failed jobs of an *inconclusive* run and hold the task.
    ///
    /// An inconclusive run — every blocking required check cancelled or never
    /// executed — carries no verdict about the code, so the correct response is
    /// to run it again, not to dispatch a remediation attempt against code that
    /// was never shown to be broken.
    ///
    /// Deduped per `(task, head SHA, run id)`. GitHub reuses the run id for a
    /// re-run, so one retrigger per run id per head is exactly one retry; if the
    /// re-run is *also* inconclusive the task holds in `pr_draft` for the normal
    /// stall watchdogs rather than looping against a sick runner fleet.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn retrigger_inconclusive_run(
        &mut self,
        gh_client: &GitHubApiClient,
        task_id: &str,
        task_short_id: &str,
        head_sha: &str,
        owner: &str,
        repo: &str,
        pull_number: u64,
        checks: &djinn_provider::github_api::CheckRunsResponse,
    ) {
        // Every distinct Actions run that contributed a non-successful check.
        let mut run_ids: Vec<u64> = Vec::new();
        for cr in &checks.check_runs {
            if ci_helpers::is_failing_conclusion(cr.conclusion.as_deref())
                && let Some(rid) = parse_actions_run_id(&cr.html_url)
                && !run_ids.contains(&rid)
            {
                run_ids.push(rid);
            }
        }

        if run_ids.is_empty() {
            tracing::warn!(
                task_id = %task_short_id,
                pr = pull_number,
                "PR poller: inconclusive CI run has no parseable Actions run id — \
                 cannot retrigger; holding in pr_draft"
            );
            return;
        }

        for run_id in run_ids {
            let key = format!("{task_id}:{head_sha}:{run_id}");
            if !self.ci_inconclusive_retriggered.insert(key) {
                tracing::debug!(
                    task_id = %task_short_id,
                    pr = pull_number,
                    run_id,
                    "PR poller: inconclusive run already retriggered for this head — holding"
                );
                continue;
            }

            match gh_client.rerun_failed_jobs(owner, repo, run_id).await {
                Ok(()) => tracing::info!(
                    task_id = %task_short_id,
                    pr = pull_number,
                    run_id,
                    sha = &head_sha[..head_sha.len().min(12)],
                    "PR poller: retriggered failed jobs of an INCONCLUSIVE run \
                     (every blocking required check was cancelled or never executed)"
                ),
                Err(e) => tracing::warn!(
                    task_id = %task_short_id,
                    pr = pull_number,
                    run_id,
                    error = %e,
                    "PR poller: failed to retrigger inconclusive run (non-fatal); \
                     holding in pr_draft"
                ),
            }
        }
    }
}
