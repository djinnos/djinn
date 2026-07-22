//! Blind-spot merged-PR reconciliation.
//!
//! The PR poller's status-scoped loops only poll `pr_draft` and `pr_review`
//! tasks. A PR can merge — or its head CI can change — while its task sits in a
//! status the poller never observes (`open`, `in_progress`, `needs_task_review`,
//! `needs_lead_intervention`, ...). When that happens the merge is never noticed
//! and, worse, the task's promoted CI snapshot can stay frozen at `failing`,
//! which makes the respawn guard bypass open-PR adoption and dispatch rework
//! workers against an already-merged PR.
//!
//! This pass closes that blind spot. It runs on the slow stale-sweep cadence
//! (never per tick) over the bounded set of non-terminal tasks that carry a
//! `pr_url` and are NOT in a poller-owned status. For each it fetches PR state
//! via the same installation-token GitHub plumbing the poller already uses:
//!
//! - MERGED → terminalize with the same merge bookkeeping the poller applies for
//!   a `pr_review` merge (the `pr_merge` path), routed through
//!   `user_override → pr_review` first because `PrMerge` is only a valid
//!   transition from `pr_draft`/`pr_review`. This mirrors the manual
//!   `user_override → pr_merge` an operator runs today.
//! - CLOSED-unmerged → left untouched (log only); abandoned-PR closing policy is
//!   out of scope.
//! - OPEN → left untouched; the CI snapshot is refreshed in the same pass so a
//!   poisoned `failing` snapshot cannot bypass adoption forever.

use super::*;
use djinn_core::models::TaskStatus;
use djinn_provider::github_api::PrState;

/// Audit reason recorded on the `user_override → pr_review` step that routes a
/// blind-spot task into a state from which `PrMerge` can terminalize it. Names
/// the reconciliation so the audit trail shows WHY the task closed outside the
/// normal poller merge path.
const RECONCILE_MERGE_REASON: &str =
    "reconciled: PR merged while task was outside PR-poller-owned statuses \
     (blind-spot merged-PR reconciliation)";

/// The action the reconciliation pass takes for a blind-spot task, decided
/// purely from the fetched PR state. Extracted so the branch logic is testable
/// without the async GitHub-fetch machinery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlindSpotReconcileAction {
    /// PR is merged — terminalize the task with merge semantics.
    Terminalize,
    /// PR is closed without merging — leave the task alone (out of scope), log.
    LogClosedUnmerged,
    /// PR is still open — leave the task alone; refresh its CI snapshot only.
    RefreshCiOnly,
}

/// Decide the reconciliation action from the fetched PR's merged/closed state.
///
/// A merged PR is ground truth and wins even if GitHub also reports the PR as
/// closed (merged PRs are closed). Only an unmerged closed PR maps to
/// `LogClosedUnmerged`.
pub(crate) fn decide_blindspot_reconcile_action(
    merged: bool,
    state: PrState,
) -> BlindSpotReconcileAction {
    if merged {
        BlindSpotReconcileAction::Terminalize
    } else if state == PrState::Closed {
        BlindSpotReconcileAction::LogClosedUnmerged
    } else {
        BlindSpotReconcileAction::RefreshCiOnly
    }
}

impl CoordinatorActor {
    /// Reconcile merged-but-unnoticed PRs for tasks the poller's status-scoped
    /// loops never observe. Runs on the slow stale-sweep cadence; the query is
    /// bounded to non-terminal tasks with a `pr_url` that are not poller-owned.
    pub(crate) async fn reconcile_blindspot_merged_prs(&self) {
        let task_repo = self.task_repo();
        let project_repo = djinn_db::ProjectRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );

        let tasks = match task_repo.list_reconcilable_pr_tasks().await {
            Ok(tasks) => tasks,
            Err(e) => {
                tracing::warn!(error = %e, "PR reconcile: failed to query blind-spot PR tasks");
                return;
            }
        };
        if tasks.is_empty() {
            return;
        }

        tracing::debug!(
            count = tasks.len(),
            "PR reconcile: checking {} blind-spot PR task(s) for unnoticed merges",
            tasks.len()
        );

        for task in tasks {
            let Some(pr_url) = task.pr_url.as_deref() else {
                continue;
            };
            let Some((owner, repo, pull_number)) = parse_pr_url(pr_url) else {
                tracing::warn!(
                    task_id = %task.short_id,
                    pr_url,
                    "PR reconcile: unrecognised PR URL format, skipping"
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
                        "PR reconcile: no installation_id on project row; skipping (legacy project?)"
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
                        "PR reconcile: failed to fetch PR status; skipping this tick"
                    );
                    continue;
                }
            };

            match decide_blindspot_reconcile_action(pr.merged == Some(true), pr.state) {
                BlindSpotReconcileAction::Terminalize => {
                    tracing::info!(
                        task_id = %task.short_id,
                        status = %task.status,
                        pr = pull_number,
                        "PR reconcile: PR merged for blind-spot task → terminalizing with merge semantics"
                    );
                    self.terminalize_reconciled_merge(
                        &task,
                        pr_url,
                        pr.merge_commit_sha.as_deref(),
                    )
                    .await;
                }
                BlindSpotReconcileAction::LogClosedUnmerged => {
                    tracing::info!(
                        task_id = %task.short_id,
                        status = %task.status,
                        pr = pull_number,
                        "PR reconcile: PR closed without merge for blind-spot task → leaving task (abandoned-PR closing is out of scope)"
                    );
                }
                BlindSpotReconcileAction::RefreshCiOnly => {
                    // PR still open (legitimate rework in flight, etc.) — do NOT
                    // touch the task. Refresh the CI snapshot from the same fetch
                    // so a stale/poisoned `failing` snapshot cannot bypass
                    // open-PR adoption forever. `record_ci_snapshot` is the sole
                    // writer of the GitHub-derived CI fields and handles head-SHA
                    // change resets internally.
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
                }
            }
        }
    }

    /// Terminalize a blind-spot task whose PR has merged, using the poller's
    /// standard merge bookkeeping (`apply_pr_merge`). `PrMerge` is only valid
    /// from `pr_draft`/`pr_review`, so the task is first routed into `pr_review`
    /// via `user_override` (the reconciliation-named audit step) — the same
    /// two-step an operator performs manually. On override failure the merge is
    /// skipped (retried next slow tick) rather than leaving a half-applied state.
    async fn terminalize_reconciled_merge(
        &self,
        task: &Task,
        pr_url: &str,
        merge_commit_sha: Option<&str>,
    ) {
        if let Err(e) = self
            .task_repo()
            .transition(
                &task.id,
                TransitionAction::UserOverride,
                "system",
                "pr_poller",
                Some(RECONCILE_MERGE_REASON),
                Some(TaskStatus::PrReview),
            )
            .await
        {
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                "PR reconcile: failed to route merged blind-spot task into pr_review; \
                 skipping terminalization this tick"
            );
            return;
        }

        // Review/merge-queue provenance is unknown on the reconciliation path
        // (the merge landed unobserved). Record `not_applicable` — the same
        // value the pr_draft merged path uses — so the merge facts are recorded
        // without asserting a review outcome we did not witness.
        self.apply_pr_merge(
            &task.id,
            pr_url,
            merge_commit_sha,
            Some("not_applicable"),
            "not_applicable",
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::events::EventBus;
    use djinn_core::models::{CiStatus, TaskPrCiSnapshotInput};
    use djinn_db::{Database, EpicRepository, TaskRepository};

    async fn seed_task(
        repo: &TaskRepository,
        epic_id: &str,
        title: &str,
        status: &str,
        pr_url: Option<&str>,
    ) -> Task {
        let task = repo
            .create(epic_id, title, "", "", "task", 0, "", Some(status))
            .await
            .unwrap();
        match pr_url {
            Some(url) => repo.set_pr_url(&task.id, url).await.unwrap(),
            None => task,
        }
    }

    // ── Pure decision function ──────────────────────────────────────────────

    #[test]
    fn merged_pr_terminalizes_even_when_reported_closed() {
        // A merged PR is ground truth (merged PRs are also closed on GitHub);
        // it must map to Terminalize regardless of the closed flag.
        assert_eq!(
            decide_blindspot_reconcile_action(true, PrState::Closed),
            BlindSpotReconcileAction::Terminalize
        );
        assert_eq!(
            decide_blindspot_reconcile_action(true, PrState::Open),
            BlindSpotReconcileAction::Terminalize
        );
    }

    #[test]
    fn closed_unmerged_logs_only() {
        assert_eq!(
            decide_blindspot_reconcile_action(false, PrState::Closed),
            BlindSpotReconcileAction::LogClosedUnmerged
        );
    }

    #[test]
    fn open_pr_refreshes_ci_only() {
        assert_eq!(
            decide_blindspot_reconcile_action(false, PrState::Open),
            BlindSpotReconcileAction::RefreshCiOnly
        );
    }

    // ── Blind-spot query bounds ─────────────────────────────────────────────

    #[tokio::test]
    async fn list_reconcilable_excludes_poller_owned_terminal_and_prless() {
        let db = Database::open_in_memory().unwrap();
        let bus = EventBus::noop();
        let epic_repo = EpicRepository::new(db.clone(), bus.clone());
        let epic = epic_repo.create("recon epic", "", "", "", "", None).await.unwrap();
        let repo = TaskRepository::new(db.clone(), bus);

        // Blind-spot tasks with a PR — must be returned.
        let open = seed_task(&repo, &epic.id, "open+pr", "open", Some("https://github.com/acme/repo/pull/1")).await;
        let in_progress = seed_task(&repo, &epic.id, "inprog+pr", "in_progress", Some("https://github.com/acme/repo/pull/2")).await;
        let ntr = seed_task(&repo, &epic.id, "ntr+pr", "needs_task_review", Some("https://github.com/acme/repo/pull/3")).await;
        // Poller-owned statuses — the pr_draft/pr_review loops own these.
        let _draft = seed_task(&repo, &epic.id, "draft+pr", "pr_draft", Some("https://github.com/acme/repo/pull/4")).await;
        let _review = seed_task(&repo, &epic.id, "review+pr", "pr_review", Some("https://github.com/acme/repo/pull/5")).await;
        // Blind-spot status but no PR reference — nothing to reconcile.
        let _open_noprl = seed_task(&repo, &epic.id, "open+noprl", "open", None).await;

        let got: std::collections::HashSet<String> = repo
            .list_reconcilable_pr_tasks()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();

        assert!(got.contains(&open.id));
        assert!(got.contains(&in_progress.id));
        assert!(got.contains(&ntr.id));
        assert_eq!(
            got.len(),
            3,
            "only non-terminal, non-poller-owned tasks with a pr_url must be returned"
        );
    }

    // ── Merge terminalization via the state machine ─────────────────────────
    //
    // Building a full `CoordinatorActor` to drive `terminalize_reconciled_merge`
    // is too heavyweight for a unit test (see `pr_poller/tests.rs`). The
    // load-bearing claim — that a blind-spot status can be closed with merge
    // semantics via the exact `user_override → pr_review` then `pr_merge`
    // sequence the reconciler uses — is validated directly against the state
    // machine and merge bookkeeping below.
    #[tokio::test]
    async fn open_task_with_merged_pr_closes_with_merge_semantics_and_audit() {
        let db = Database::open_in_memory().unwrap();
        let bus = EventBus::noop();
        let epic_repo = EpicRepository::new(db.clone(), bus.clone());
        let epic = epic_repo.create("recon epic", "", "", "", "", None).await.unwrap();
        let repo = TaskRepository::new(db.clone(), bus);

        let task = seed_task(
            &repo,
            &epic.id,
            "merged-while-open",
            "open",
            Some("https://github.com/acme/repo/pull/7"),
        )
        .await;

        // Poison the CI snapshot to `failing` — the real respawn-guard trap the
        // reconciliation clears by terminalizing the task.
        repo.upsert_ci_snapshot(TaskPrCiSnapshotInput {
            task_id: task.id.clone(),
            pr_number: 7,
            head_sha: "deadbeef".into(),
            ci_status: CiStatus::Failing,
            blocking_required_check_names: vec!["ci".into()],
            failure_fingerprint: Some("fp".into()),
            same_signature_count: 0,
            last_remediation_base_sha: None,
        })
        .await
        .unwrap();

        // The reconciler's terminalization sequence.
        repo.transition(
            &task.id,
            TransitionAction::UserOverride,
            "system",
            "pr_poller",
            Some(RECONCILE_MERGE_REASON),
            Some(TaskStatus::PrReview),
        )
        .await
        .unwrap();
        repo.set_merge_commit_sha(&task.id, "cafef00d").await.unwrap();
        let closed = repo
            .transition(
                &task.id,
                TransitionAction::PrMerge,
                "system",
                "pr_poller",
                None,
                None,
            )
            .await
            .unwrap();

        assert_eq!(closed.status, "closed");
        assert_eq!(closed.merge_commit_sha.as_deref(), Some("cafef00d"));

        let activity = repo.list_activity(&task.id).await.unwrap();
        assert!(
            activity
                .iter()
                .any(|a| a.payload.contains("blind-spot merged-PR reconciliation")),
            "the user_override audit entry must name the reconciliation"
        );
    }
}
