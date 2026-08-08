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

        // ── Completeness gate (proposal `nafu`; NOT part of the route layer) ──
        //
        // This corrects the *existing* merge gate, so it applies to every
        // snapshot whether or not the evidence was ever routed. An enumeration the
        // provider could not finish is a prefix, and a prefix whose fetched
        // members all passed classifies below as `Passing` — a green merge
        // verdict for a set that may contain the one causal failure nobody saw.
        // The empty prefix left by a failed *first* page is worse still: it is
        // byte-identical to "this repository has no CI" and reaches both lanes'
        // fast path to green.
        //
        // Return `Unknown` and write nothing. `Unknown` maps to `Hold` at the
        // gate, the two lane fast paths below check completeness before acting
        // on emptiness, and the next poll re-reads. No counter, no synthetic
        // identity, no route row — an incomplete read converts an unproven
        // result into a wait and nothing else.
        if let Some(reason) = checks.completeness.incomplete_reason() {
            tracing::warn!(
                task_id = %task_short_id,
                pr = pull_number,
                head_sha = &head_sha[..head_sha.len().min(12)],
                collected = checks.check_runs.len(),
                total_count = checks.total_count,
                ?reason,
                "PR poller: check-run enumeration is INCOMPLETE — holding without recording a CI verdict"
            );
            return CiStatus::Unknown;
        }

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

/// Complete terminal evidence identity (proposal `nafu`, wave 2).
///
/// The classifier in [`super::ci_routing`] may only run "after the provider
/// reports a terminal run and the poller captures the complete
/// required/blocking-check set for that exact immutable evidence identity".
/// Everything in here decides whether that precondition holds, and names the
/// immutable identity when it does.
///
/// Completeness is a *proof obligation*, not a default. Every function returns
/// a reason rather than a bool, because "we could not prove the set is
/// complete" and "the set is complete and empty" are different facts that
/// route differently, and a bool loses the difference.
///
/// Consumed by the wave-3 lane executors; the `dead_code` allow is scoped to
/// this submodule so it never masks rot in the snapshot writer above.
/// The `dead_code` allow is scoped to this submodule so it never masks rot in
/// the snapshot writer above. It stays until the two lane executors consume
/// these functions from the polling loops; wave 3 landed the classifier
/// contract, the completeness verdict, and the provider-action scope, but not
/// the executors themselves — see the wave-3 report.
#[allow(dead_code)]
pub(crate) mod evidence {
    use super::ci_routing::{
        CiCapture, CiEvidenceIdentity, CiIncompleteReason, CiLane, CiPendingReason,
    };
    use super::{is_failing_conclusion, parse_actions_run_id};
    use djinn_provider::github_api::{
        CheckRun, CheckRunsResponse, CheckSetCompleteness, WorkflowRun,
    };

    /// The blocking evidence for one terminal Actions run.
    #[derive(Clone, Debug)]
    pub(crate) struct CiRunEvidence<'a> {
        pub identity: CiEvidenceIdentity,
        pub blocking: Vec<&'a CheckRun>,
        /// The enumeration verdict the blocking set was sliced out of.
        ///
        /// Carried per run rather than re-derived, because the verdict belongs
        /// to the *enumeration* and a per-run slice of an incomplete
        /// enumeration is not complete just because the slice looks tidy.
        pub enumeration: CheckSetCompleteness,
    }

    impl<'a> CiRunEvidence<'a> {
        /// The capture this run presents to the classifier.
        ///
        /// It goes through [`CiCapture::prove_complete`] rather than naming a
        /// variant: this module has no authority to declare a set complete, and
        /// after wave 3 it structurally cannot.
        pub(crate) fn capture(&'a self) -> CiCapture<'a> {
            CiCapture::prove_complete(self.enumeration, &self.blocking)
        }
    }

    /// What one lane's poll produced.
    #[derive(Clone, Debug)]
    pub(crate) enum CiLaneEvidence<'a> {
        /// Nothing terminal to classify yet.
        NonTerminal(CiPendingReason),
        /// Terminal, but the evidence set could not be completed. Fails closed for
        /// the whole lane: a blocking check we cannot attribute or enumerate may
        /// be the causal one, so no per-run subset can be called complete either.
        Incomplete(CiIncompleteReason),
        /// The enumeration was provably complete and contained nothing blocking.
        /// Lane-specific compatibility behaviour; no remediation state.
        CompleteEmpty,
        /// One entry per distinct terminal Actions run that contributed a blocking
        /// failure, ordered by run id so the routes a poll produces are stable.
        Runs(Vec<CiRunEvidence<'a>>),
    }

    impl CiLaneEvidence<'_> {
        /// The capture for a lane that produced no per-run evidence.
        ///
        /// A lane executor loops over `Runs`; the other three variants have no
        /// run to loop over and still need a classification input, and this is
        /// the only place that mapping is written.
        pub(crate) fn lane_capture(&self) -> Option<CiCapture<'static>> {
            match self {
                Self::NonTerminal(reason) => Some(CiCapture::non_terminal(*reason)),
                Self::Incomplete(reason) => Some(CiCapture::incomplete(*reason)),
                Self::CompleteEmpty => Some(CiCapture::prove_complete(
                    CheckSetCompleteness::Complete,
                    &[],
                )),
                Self::Runs(_) => None,
            }
        }
    }

    /// The Actions run a check run belongs to.
    ///
    /// Prefers the provider's own `run_id` when GitHub supplied it and falls back
    /// to the existing [`parse_actions_run_id`] URL parse, which is the same
    /// attribution `blocking_failed_checks` already uses to decide which failing
    /// checks share a required run.
    pub(crate) fn check_run_actions_run_id(cr: &CheckRun) -> Option<u64> {
        cr.run_id.or_else(|| parse_actions_run_id(&cr.html_url))
    }

    /// Whether every blocking check reached a terminal status.
    pub(crate) fn blocking_terminality(blocking: &[&CheckRun]) -> Option<CiPendingReason> {
        blocking
            .iter()
            .any(|cr| cr.status != "completed")
            .then_some(CiPendingReason::RequiredCheckNonTerminal)
    }

    /// Capture the PR-head lane's complete terminal evidence.
    ///
    /// One route per distinct Actions run, because the immutable evidence identity
    /// the proposal defines names a single `run_id` and the Tier-1 provider action
    /// is `rerun_failed_jobs(owner, repo, run_id)` — also singular. A PR head with
    /// two failing workflow runs is therefore two evidence identities with two
    /// action keys, which is what lets a genuinely new second run get its own
    /// call episode instead of colliding with the first.
    ///
    /// A blocking check that cannot be attributed to any run makes the *whole
    /// lane* incomplete rather than being dropped: it may be the causal check, and
    /// dropping it would let a run that never reached a verdict look inconclusive.
    pub(crate) fn capture_pr_head_evidence<'a>(
        pr_number: i64,
        pr_head_sha: &str,
        checks: &CheckRunsResponse,
        blocking: &[&'a CheckRun],
    ) -> CiLaneEvidence<'a> {
        // `CiCapture::prove_complete` owns the provider-reason translation;
        // asking it here is what keeps the lane from forking that table.
        if let Some(reason) = enumeration_reason(checks) {
            return CiLaneEvidence::Incomplete(reason);
        }
        if let Some(reason) = blocking_terminality(blocking) {
            return CiLaneEvidence::NonTerminal(reason);
        }
        // An authoritatively complete enumeration with nothing blocking is the
        // proposal's complete-empty row, and it is answered *before* run
        // attribution: there is no run to attribute, and treating "no run ids"
        // as `RunAttributionUnavailable` would have sent every no-CI repository
        // to Tier 2.
        if blocking.is_empty() {
            return CiLaneEvidence::CompleteEmpty;
        }
        // The whole lane fails closed on unusable execution evidence rather
        // than fanning out and letting each run answer for itself: a check
        // whose interval contradicts its own conclusion may be the causal one,
        // and a per-run slice that happens to exclude it is not thereby
        // complete. `CiCapture::prove_complete` re-checks this per run — the
        // duplication is deliberate defence in depth, and both sides call the
        // same function so they cannot disagree.
        if let Some(reason) = super::ci_routing::blocking_evidence_completeness(blocking) {
            return CiLaneEvidence::Incomplete(reason);
        }
        if blocking
            .iter()
            .any(|cr| check_run_actions_run_id(cr).is_none())
        {
            return CiLaneEvidence::Incomplete(CiIncompleteReason::RunAttributionUnavailable);
        }

        let mut run_ids: Vec<u64> = blocking
            .iter()
            .filter_map(|cr| check_run_actions_run_id(cr))
            .collect();
        run_ids.sort_unstable();
        run_ids.dedup();

        let runs = run_ids
            .into_iter()
            .map(|run_id| CiRunEvidence {
                identity: CiEvidenceIdentity {
                    lane: CiLane::PrHead,
                    pr_number,
                    pr_head_sha: pr_head_sha.to_owned(),
                    run_id: Some(i64::try_from(run_id).unwrap_or(i64::MAX)),
                    // The PR-head lane's run head SHA *is* the PR head: the checks
                    // were enumerated for that ref.
                    run_head_sha: pr_head_sha.to_owned(),
                    dequeue_id: None,
                },
                blocking: blocking
                    .iter()
                    .filter(|cr| check_run_actions_run_id(cr) == Some(run_id))
                    .copied()
                    .collect(),
                enumeration: checks.completeness,
            })
            .collect();

        CiLaneEvidence::Runs(runs)
    }

    /// The classifier-side reason for an incomplete enumeration, or `None` when
    /// the enumeration is provably complete.
    ///
    /// Delegates to [`CiCapture::prove_complete`] rather than repeating the
    /// provider-reason mapping, so there is exactly one translation table in
    /// the crate. The probe set is empty because the enumeration verdict is
    /// checked first and short-circuits before the blocking set is consulted.
    pub(crate) fn enumeration_reason(checks: &CheckRunsResponse) -> Option<CiIncompleteReason> {
        CiCapture::prove_complete(checks.completeness, &[]).incomplete_reason()
    }

    /// Correlate a merge-group dequeue to exactly one terminal merge-group run.
    ///
    /// The queue branch is `gh-readonly-queue/<base>/pr-<number>-<sha>`, so the
    /// `pr-<number>-` marker (with its trailing dash, which is what stops `pr-1-`
    /// from matching `pr-11-`) identifies the PR. Ambiguity is a first-class
    /// answer here rather than "take the newest", which is what the legacy
    /// enrichment path does: two terminal merge-group runs for one PR means the
    /// queue ran the PR twice and we cannot say which one this dequeue refers to,
    /// and the proposal lists exactly that as an unknown case that fails closed.
    /// Why a merge-group correlation did not yield exactly one terminal run.
    ///
    /// The two arms route differently and must not be one error:
    ///
    /// * **`NotTerminal` holds.** The proposal's table says "run or any
    ///   required check is pending/non-terminal → Hold; wait for a terminal
    ///   snapshot, do not classify". A merge-group run that correlates
    ///   perfectly but has not finished is precisely that row. Wave 2 mapped it
    ///   to `MergeGroupCorrelationUnavailable`, which is an *unknown-evidence*
    ///   reason and therefore a guarded Tier 2 — a Lead session and a route row
    ///   spent on a run that is simply still going, and which the next poll
    ///   would have classified for free.
    /// * **`Unusable` fails closed**, because a merge group we cannot name is
    ///   not a wait. Its two reasons then part company at the classifier, and
    ///   the difference is which identity exists:
    ///   * `AmbiguousMergeGroupCorrelation` — two or more terminal runs were
    ///     named and we cannot say which this dequeue refers to. "Ambiguous" is
    ///     stable: re-asking returns the same several runs. So it is
    ///     **irrecoverable** and takes one diagnose-only route under the
    ///     run-absent identity, carrying its real dequeue id — only the run is
    ///     absent, and absence is spelled `None`, never a sentinel.
    ///   * `MergeGroupCorrelationUnavailable` — *no* run was named **yet**. The
    ///     queue run can appear on a later poll at no cost, so it is
    ///     **recoverable** and takes the bounded hold with no route row. See
    ///     [`CiIncompleteReason::recoverability`].
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum CiMergeGroupCorrelationError {
        NotTerminal(CiPendingReason),
        Unusable(CiIncompleteReason),
    }

    pub(crate) fn correlate_merge_group_run(
        pr_number: u64,
        runs: &[WorkflowRun],
    ) -> Result<&WorkflowRun, CiMergeGroupCorrelationError> {
        let marker = format!("pr-{pr_number}-");
        let candidates: Vec<&WorkflowRun> = runs
            .iter()
            .filter(|r| {
                r.head_branch
                    .as_deref()
                    .is_some_and(|b| b.contains(&marker))
                    && is_failing_conclusion(r.conclusion.as_deref())
            })
            .collect();

        match candidates.as_slice() {
            [] => Err(CiMergeGroupCorrelationError::Unusable(
                CiIncompleteReason::MergeGroupCorrelationUnavailable,
            )),
            [only] => {
                if only.status.as_deref() == Some("completed") {
                    Ok(only)
                } else {
                    Err(CiMergeGroupCorrelationError::NotTerminal(
                        CiPendingReason::RunNonTerminal,
                    ))
                }
            }
            _ => Err(CiMergeGroupCorrelationError::Unusable(
                CiIncompleteReason::AmbiguousMergeGroupCorrelation,
            )),
        }
    }

    impl From<CiMergeGroupCorrelationError> for CiLaneEvidence<'_> {
        fn from(err: CiMergeGroupCorrelationError) -> Self {
            match err {
                CiMergeGroupCorrelationError::NotTerminal(reason) => {
                    CiLaneEvidence::NonTerminal(reason)
                }
                CiMergeGroupCorrelationError::Unusable(reason) => {
                    CiLaneEvidence::Incomplete(reason)
                }
            }
        }
    }

    /// The dequeue event's identity.
    ///
    /// A merge-group route is only as identifiable as the dequeue that produced
    /// it, and `DequeueEvent` has no id of its own. The merge-group ref plus the
    /// event timestamp is the most specific pair GitHub gives us, and a route that
    /// cannot name both cannot prove on a later poll that it is still looking at
    /// the same dequeue — so it returns `None` and the lane fails closed.
    pub(crate) fn dequeue_identity(
        dequeue: &djinn_provider::github_api::DequeueEvent,
    ) -> Option<String> {
        let created_at = dequeue.created_at.as_deref()?;
        let group_ref = dequeue.merge_group_ref.as_deref()?;
        Some(format!("{group_ref}@{created_at}"))
    }

    /// Capture the merge-group lane's complete terminal evidence for one dequeue.
    pub(crate) fn capture_merge_group_evidence<'a>(
        pr_number: i64,
        pr_head_sha: &str,
        run: &WorkflowRun,
        dequeue_id: &str,
        checks: &CheckRunsResponse,
        blocking: &[&'a CheckRun],
    ) -> CiLaneEvidence<'a> {
        if let Some(reason) = enumeration_reason(checks) {
            return CiLaneEvidence::Incomplete(reason);
        }
        if let Some(reason) = blocking_terminality(blocking) {
            return CiLaneEvidence::NonTerminal(reason);
        }
        if blocking.is_empty() {
            return CiLaneEvidence::CompleteEmpty;
        }
        if let Some(reason) = super::ci_routing::blocking_evidence_completeness(blocking) {
            return CiLaneEvidence::Incomplete(reason);
        }

        CiLaneEvidence::Runs(vec![CiRunEvidence {
            identity: CiEvidenceIdentity {
                lane: CiLane::MergeGroup,
                pr_number,
                pr_head_sha: pr_head_sha.to_owned(),
                run_id: Some(i64::try_from(run.id).unwrap_or(i64::MAX)),
                run_head_sha: run.head_sha.clone(),
                dequeue_id: Some(dequeue_id.to_owned()),
            },
            blocking: blocking.to_vec(),
            enumeration: checks.completeness,
        }])
    }
}
