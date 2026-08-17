//! Merged-PR reconciliation (the poller's safety net).
//!
//! A PR can merge — or its head CI can change — without the status-scoped
//! poller loops terminalizing the task. Two shapes cause that:
//!
//! 1. The task sits in a status the poller never observes (`open`,
//!    `in_progress`, `needs_task_review`, `needs_lead_intervention`, ...). The
//!    merge is never noticed and, worse, the task's promoted CI snapshot can
//!    stay frozen at `failing`, which makes the respawn guard bypass open-PR
//!    adoption and dispatch rework workers against an already-merged PR.
//! 2. The task IS in a poller-owned status (`pr_draft` / `pr_review`) but the
//!    owning loop `continue`d past its merge check. This is not hypothetical:
//!    incidents `4vnt`/#3153 and `3kza`/#3155 both stranded in `pr_draft`
//!    because an active tripwire hold short-circuited the `pr_draft` loop
//!    *before* its merged branch (see `pr_watcher::poll_pr_draft_tasks`). The
//!    primary defect is fixed there, but this pass must cover those statuses
//!    too: a net with a hole exactly where the primary mechanism operates is
//!    not a net. A task that never terminalizes never releases its dependents,
//!    so the blast radius is a silently-stalled dependency chain, not a wrong
//!    status column.
//!
//! The pass runs on the slow stale-sweep cadence (never per tick) over the
//! bounded set of non-terminal tasks that carry a `pr_url`. For each it fetches
//! PR state via the same installation-token GitHub plumbing the poller uses:
//!
//! - MERGED → terminalize with the same merge bookkeeping the poller applies for
//!   a `pr_review` merge (the `pr_merge` path). From `pr_draft`/`pr_review` the
//!   `PrMerge` transition applies directly; from any other status the task is
//!   first routed through `user_override → pr_review` because `PrMerge` is only
//!   valid from those two. This mirrors the manual `user_override → pr_merge`
//!   an operator runs today.
//! - CLOSED-unmerged → left untouched (log only); abandoned-PR closing policy is
//!   out of scope. The poller-owned loops already force-close this case
//!   themselves.
//! - OPEN → left untouched. For blind-spot statuses the CI snapshot is refreshed
//!   in the same pass so a poisoned `failing` snapshot cannot bypass adoption
//!   forever; for poller-owned statuses the owning loop refreshes it every tick,
//!   so this pass does not duplicate the provider fan-out.
//!
//! Terminalization is idempotent by construction: the query excludes `closed`,
//! and the task row is re-read immediately before the transition, so a task the
//! poller closed in the same tick is skipped rather than double-transitioned.

use super::*;
use djinn_core::models::TaskStatus;
use djinn_provider::github_api::PrState;

/// Audit reason recorded on the transition that terminalizes a task outside the
/// normal poller merge path — on the `user_override → pr_review` routing step
/// for a blind-spot status, or directly on the `pr_merge` step when the task is
/// already in `pr_draft`/`pr_review`. Names the reconciliation so the audit
/// trail shows WHY the task closed outside the normal poller merge path.
const RECONCILE_MERGE_REASON: &str = "reconciled: PR merged while task was outside PR-poller-owned statuses \
     (blind-spot merged-PR reconciliation)";

/// Task statuses the PR poller's status-scoped loops own and poll every tick.
///
/// The reconciliation pass still *covers* these (a miss in the owning loop is
/// exactly what stranded `4vnt` and `3kza`), but it declines the non-terminal
/// side effects — CI-snapshot refresh — for them, because the owning loop
/// already performs those on its own cadence.
pub(crate) fn status_is_poller_owned(status: &str) -> bool {
    matches!(status, "pr_draft" | "pr_review")
}

/// GitHub's terminal verdict on a PR, independent of any Djinn-side gate.
///
/// This is the single classifier both the `pr_draft` poll loop
/// (`pr_watcher::poll_pr_draft_tasks`) and this reconciliation pass consult, so
/// the primary path and the safety net cannot drift into disagreeing about what
/// "merged" means. A merged PR is ground truth: the work is on the base branch
/// and no Djinn-side gate can un-land it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrTerminalState {
    /// The PR merged. Ground truth — outranks every Djinn-side gate.
    Merged,
    /// The PR was closed without merging.
    ClosedUnmerged,
    /// The PR is still open.
    Live,
}

/// Classify a fetched PR's terminal state.
///
/// `merged` wins over `state` unconditionally: GitHub reports merged PRs as
/// closed, so a `Closed` state says nothing on its own. Deliberately takes no
/// SHA argument — merge detection must never depend on a stored head SHA, which
/// a force-push invalidates (both incident branches were force-pushed during a
/// rebase, leaving `task_attempts.github_head_sha` stale against the merged
/// head).
pub(crate) fn classify_pr_terminal_state(merged: bool, state: PrState) -> PrTerminalState {
    if merged {
        PrTerminalState::Merged
    } else if state == PrState::Closed {
        PrTerminalState::ClosedUnmerged
    } else {
        PrTerminalState::Live
    }
}

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
/// `LogClosedUnmerged`. Delegates to [`classify_pr_terminal_state`], the same
/// classifier the `pr_draft` poll loop uses.
pub(crate) fn decide_blindspot_reconcile_action(
    merged: bool,
    state: PrState,
) -> BlindSpotReconcileAction {
    match classify_pr_terminal_state(merged, state) {
        PrTerminalState::Merged => BlindSpotReconcileAction::Terminalize,
        PrTerminalState::ClosedUnmerged => BlindSpotReconcileAction::LogClosedUnmerged,
        PrTerminalState::Live => BlindSpotReconcileAction::RefreshCiOnly,
    }
}

impl CoordinatorActor {
    /// Reconcile merged-but-unnoticed PRs. Runs on the slow stale-sweep
    /// cadence; the query is bounded to non-terminal tasks with a `pr_url`,
    /// poller-owned statuses included (see the module doc — excluding them is
    /// what let `4vnt` and `3kza` strand).
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
            if !self.task_pr_handling_is_eligible(&task).await {
                continue;
            }
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
            crate::direct_delivery::observe_boundary_operation("task_pr_merged_poll");

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
                    //
                    // Except for poller-owned statuses: their loop records the
                    // same snapshot from its own fetch every tick, so repeating
                    // it here would only duplicate the provider fan-out
                    // (required contexts, merge-group correlation) on the slow
                    // sweep. Coverage of those statuses exists for the MERGED
                    // ground-truth case, not to take over live CI observation.
                    if status_is_poller_owned(&task.status) {
                        continue;
                    }
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

    /// Terminalize a task whose PR has merged, using the poller's standard
    /// merge bookkeeping (`apply_pr_merge`).
    ///
    /// `PrMerge` is only valid from `pr_draft`/`pr_review`:
    ///
    /// - Already in one of those (the `4vnt`/`3kza` shape — the owning loop
    ///   missed the merge): apply `PrMerge` directly, carrying
    ///   [`RECONCILE_MERGE_REASON`] as the transition reason so the audit trail
    ///   still names the reconciliation. Routing through `user_override` first
    ///   would emit a pointless extra status change and an extra audit row.
    /// - Any other (blind-spot) status: route into `pr_review` via
    ///   `user_override` carrying the same reason — the same two-step an
    ///   operator performs manually. On override failure the merge is skipped
    ///   (retried next slow tick) rather than leaving a half-applied state.
    ///
    /// Idempotent: the task row is re-read first, and a task that already
    /// reached `closed` (the poller terminalized it between the query and here)
    /// is left alone. Without that re-read the two paths race on the same merge
    /// and the loser logs a failed illegal transition every slow tick.
    async fn terminalize_reconciled_merge(
        &self,
        task: &Task,
        pr_url: &str,
        merge_commit_sha: Option<&str>,
    ) {
        let task_repo = self.task_repo();
        let current = match task_repo.get(&task.id).await {
            Ok(Some(current)) => current,
            Ok(None) => {
                tracing::warn!(
                    task_id = %task.short_id,
                    "PR reconcile: task disappeared before merge terminalization"
                );
                return;
            }
            Err(e) => {
                tracing::warn!(
                    task_id = %task.short_id,
                    error = %e,
                    "PR reconcile: failed to re-read task before merge terminalization; \
                     skipping this tick"
                );
                return;
            }
        };
        if current.status == TaskStatus::Closed.as_str() {
            tracing::debug!(
                task_id = %task.short_id,
                "PR reconcile: task already terminal (poller won the race) — nothing to do"
            );
            return;
        }

        // `PrMerge` needs `pr_draft`/`pr_review`. Route anything else there
        // first; a task already in one of them merges directly.
        if !status_is_poller_owned(&current.status)
            && let Err(e) = task_repo
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
        self.apply_pr_merge_with_reason(
            &task.id,
            pr_url,
            merge_commit_sha,
            Some("not_applicable"),
            "not_applicable",
            Some(RECONCILE_MERGE_REASON),
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

    #[test]
    fn a_merged_pr_outranks_every_djinn_side_gate() {
        // The 4vnt/3kza root cause, stated as a classifier contract: nothing
        // about a merged PR is conditional. `classify_pr_terminal_state` takes
        // no gate state and no SHA, so neither an active tripwire hold nor a
        // force-pushed (stale) stored head can produce anything but `Merged`.
        assert_eq!(
            classify_pr_terminal_state(true, PrState::Closed),
            PrTerminalState::Merged
        );
        assert_eq!(
            classify_pr_terminal_state(true, PrState::Open),
            PrTerminalState::Merged
        );
        assert_eq!(
            classify_pr_terminal_state(false, PrState::Closed),
            PrTerminalState::ClosedUnmerged
        );
        assert_eq!(
            classify_pr_terminal_state(false, PrState::Open),
            PrTerminalState::Live
        );
    }

    #[test]
    fn poller_owned_statuses_are_exactly_pr_draft_and_pr_review() {
        assert!(status_is_poller_owned("pr_draft"));
        assert!(status_is_poller_owned("pr_review"));
        for other in [
            "open",
            "in_progress",
            "needs_task_review",
            "needs_lead_intervention",
            "approved",
            "closed",
        ] {
            assert!(
                !status_is_poller_owned(other),
                "{other} is not a poller-owned status"
            );
        }
    }

    // ── Reconciliation query bounds ─────────────────────────────────────────

    /// REGRESSION (`4vnt`/#3153, `3kza`/#3155): the reconciliation query used
    /// to exclude `pr_draft`/`pr_review` — the exact statuses both stranded
    /// tasks were in — leaving the safety net with a hole precisely where the
    /// primary mechanism operates. Every non-terminal task with a `pr_url` must
    /// now be reconcilable, whichever status it sits in.
    #[tokio::test]
    async fn list_reconcilable_covers_poller_owned_statuses() {
        let db = Database::open_in_memory().unwrap();
        let bus = EventBus::noop();
        let epic_repo = EpicRepository::new(db.clone(), bus.clone());
        let epic = epic_repo
            .create("recon epic", "", "", "", "", None)
            .await
            .unwrap();
        let repo = TaskRepository::new(db.clone(), bus);

        // Blind-spot tasks with a PR — must be returned.
        let open = seed_task(
            &repo,
            &epic.id,
            "open+pr",
            "open",
            Some("https://github.com/acme/repo/pull/1"),
        )
        .await;
        let in_progress = seed_task(
            &repo,
            &epic.id,
            "inprog+pr",
            "in_progress",
            Some("https://github.com/acme/repo/pull/2"),
        )
        .await;
        let ntr = seed_task(
            &repo,
            &epic.id,
            "ntr+pr",
            "needs_task_review",
            Some("https://github.com/acme/repo/pull/3"),
        )
        .await;
        // Poller-owned statuses — the incident shape. The owning loop can miss
        // a merge (it did, twice), so these MUST be covered too.
        let draft = seed_task(
            &repo,
            &epic.id,
            "draft+pr",
            "pr_draft",
            Some("https://github.com/acme/repo/pull/4"),
        )
        .await;
        let review = seed_task(
            &repo,
            &epic.id,
            "review+pr",
            "pr_review",
            Some("https://github.com/acme/repo/pull/5"),
        )
        .await;
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
        assert!(
            got.contains(&draft.id),
            "a pr_draft task with a merged-but-unnoticed PR is the 4vnt/3kza shape; \
             excluding it is the safety-net hole this fix closes"
        );
        assert!(
            got.contains(&review.id),
            "pr_review has the same exposure as pr_draft"
        );
        assert_eq!(
            got.len(),
            5,
            "every non-terminal task with a pr_url must be reconcilable"
        );
    }

    /// A task the poller already terminalized must never be re-selected: the
    /// query is the first (and cheapest) idempotency barrier.
    #[tokio::test]
    async fn list_reconcilable_excludes_terminalized_tasks() {
        let db = Database::open_in_memory().unwrap();
        let bus = EventBus::noop();
        let epic_repo = EpicRepository::new(db.clone(), bus.clone());
        let epic = epic_repo
            .create("recon epic", "", "", "", "", None)
            .await
            .unwrap();
        let repo = TaskRepository::new(db.clone(), bus);

        let task = seed_task(
            &repo,
            &epic.id,
            "already-merged",
            "pr_draft",
            Some("https://github.com/acme/repo/pull/9"),
        )
        .await;
        assert_eq!(repo.list_reconcilable_pr_tasks().await.unwrap().len(), 1);

        // The poller wins the race and closes it with merge semantics.
        repo.transition(
            &task.id,
            TransitionAction::PrMerge,
            "system",
            "pr_poller",
            None,
            None,
        )
        .await
        .unwrap();

        assert!(
            repo.list_reconcilable_pr_tasks().await.unwrap().is_empty(),
            "a closed task must never be reconciled again"
        );
    }

    // ── Merge terminalization driven through the production path ───────────
    //
    // These build a real `CoordinatorActor` and call the production
    // `terminalize_reconciled_merge` — the same method
    // `reconcile_blindspot_merged_prs` calls once GitHub reports the PR merged.
    // Only the GitHub fetch is elided (there is no HTTP fake in this crate), so
    // the merge facts are passed in exactly as the fetch would supply them.

    /// Build a coordinator actor plus an epic, returning everything a
    /// terminalization test needs. Caller must `cancel.cancel()` at the end.
    async fn reconcile_fixture(
        db: &Database,
    ) -> (
        crate::actor::CoordinatorActor,
        tokio_util::sync::CancellationToken,
        TaskRepository,
        String,
    ) {
        let (tx, _rx) = tokio::sync::broadcast::channel(64);
        let (actor, cancel) = crate::test_helpers::make_coordinator_actor_cancellable(db, &tx);
        let bus = EventBus::noop();
        let epic_repo = EpicRepository::new(db.clone(), bus.clone());
        let epic = epic_repo
            .create("recon epic", "", "", "", "", None)
            .await
            .unwrap();
        let repo = TaskRepository::new(db.clone(), bus);
        (actor, cancel, repo, epic.id)
    }

    /// REGRESSION (`4vnt`/#3153, `3kza`/#3155), the incident shape end to end.
    ///
    /// A task sits in `pr_draft` — the status the poller owns — while its PR is
    /// merged. Before this fix the reconciliation query refused to look at
    /// `pr_draft` at all, so nothing terminalized it and the task stayed open
    /// with `closed_at: null` (5 days for `4vnt`). Now it closes with merge
    /// semantics: `close_reason = completed`, a populated `merge_commit_sha`
    /// (both incidents left it `null`), and an audit trail that names the
    /// reconciliation. And the dependent — the thing that actually cost the
    /// build loop — is released.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn merged_pr_in_pr_draft_terminalizes_and_releases_its_dependent() {
        let db = Database::open_in_memory().unwrap();
        let (actor, cancel, repo, epic_id) = reconcile_fixture(&db).await;

        let pr_url = "https://github.com/djinnos/djinn/pull/3153";
        let task = seed_task(
            &repo,
            &epic_id,
            "stranded-in-pr-draft",
            "pr_draft",
            Some(pr_url),
        )
        .await;
        // The dependency chain that silently dropped out of the build loop.
        let dependent = seed_task(&repo, &epic_id, "blocked-dependent", "open", None).await;
        repo.add_blocker(&dependent.id, &task.id).await.unwrap();

        let ready_before: Vec<String> = repo
            .list_ready(djinn_db::ReadyQuery::default())
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert!(
            !ready_before.contains(&dependent.id),
            "precondition: the dependent must be blocked while the merged task stays open"
        );

        // The reconciler must select it in the first place.
        let selected = repo.list_reconcilable_pr_tasks().await.unwrap();
        let selected_task = selected
            .iter()
            .find(|t| t.id == task.id)
            .expect("a pr_draft task with a merged PR must be reconcilable");
        assert_eq!(
            decide_blindspot_reconcile_action(true, PrState::Closed),
            BlindSpotReconcileAction::Terminalize
        );

        actor
            .terminalize_reconciled_merge(selected_task, pr_url, Some("0c4e0a5224be4133"))
            .await;

        let closed = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(closed.status, "closed");
        assert_eq!(closed.close_reason.as_deref(), Some("completed"));
        assert!(closed.closed_at.is_some(), "closed_at must be populated");
        assert_eq!(
            closed.merge_commit_sha.as_deref(),
            Some("0c4e0a5224be4133"),
            "a task closed as merged must record which commit merged it"
        );

        let activity = repo.list_activity(&task.id).await.unwrap();
        assert!(
            activity
                .iter()
                .any(|a| a.payload.contains("blind-spot merged-PR reconciliation")),
            "the audit trail must name why the task closed outside the normal poller path"
        );

        let ready_after: Vec<String> = repo
            .list_ready(djinn_db::ReadyQuery::default())
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert!(
            ready_after.contains(&dependent.id),
            "closing the merged task must release its blocked dependent — \
             the un-released dependency chain IS the damage this bug caused"
        );

        cancel.cancel();
        db.pool().close().await;
    }

    /// REGRESSION: the force-pushed-branch shape observed on BOTH incidents.
    ///
    /// `4vnt` held `ci.head_sha = d41958f6…` (the merged head) but
    /// `task_attempts.github_head_sha = b3aaede1…` (the pre-force-push head);
    /// `3kza` showed `fef4a526…` vs `0d67ee6e…`. Merge terminalization must not
    /// consult either stored SHA — a force-push invalidates them, and a merge
    /// check keyed off one would strand the task for a second, independent
    /// reason. Here every stored SHA disagrees with the merge commit and the
    /// task must still terminalize.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn force_pushed_stale_head_shas_do_not_block_terminalization() {
        use djinn_db::{CreateTaskAttemptParams, FillTaskAttemptParams, TaskAttemptRepository};

        let db = Database::open_in_memory().unwrap();
        let (actor, cancel, repo, epic_id) = reconcile_fixture(&db).await;

        let pr_url = "https://github.com/djinnos/djinn/pull/3155";
        let task = seed_task(&repo, &epic_id, "force-pushed", "pr_draft", Some(pr_url)).await;

        // Snapshot head: the merged head. Attempt head: the stale pre-rebase
        // head. Exactly the divergence both production tasks carried.
        const MERGED_HEAD: &str = "fef4a526f115c7e5ca6b86cf76856676389f176f";
        const STALE_ATTEMPT_HEAD: &str = "0d67ee6e892124e38221a515b466b3b7a582f4d1";
        const MERGE_COMMIT: &str = "06253f5ab0b48f14769598a8cded90a013d90206";

        repo.upsert_ci_snapshot(TaskPrCiSnapshotInput {
            task_id: task.id.clone(),
            pr_number: 3155,
            head_sha: MERGED_HEAD.into(),
            ci_status: CiStatus::Passing,
            blocking_required_check_names: vec![],
            primary_blocking_check: None,
            failure_annotations: None,
            failure_fingerprint: None,
            same_signature_count: 0,
            last_remediation_base_sha: None,
        })
        .await
        .unwrap();

        let attempts = TaskAttemptRepository::new(db.clone());
        let attempt = attempts
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &uuid::Uuid::now_v7().to_string(),
                task_id: &task.id,
                role: "worker",
                dispatch_key: &format!("dk-{}", uuid::Uuid::now_v7()),
                session_id: None,
                attempt_seq: None,
                dispatch_owner_incarnation_id: None,
                dispatch_group_id: None,
            })
            .await
            .unwrap();
        attempts
            .fill_nullable_fields(FillTaskAttemptParams {
                id: &attempt.id,
                checkpoint_ref: None,
                submit_ref: None,
                pr_url: None,
                mirror_head_sha: Some(STALE_ATTEMPT_HEAD),
                github_head_sha: Some(STALE_ATTEMPT_HEAD),
                github_publication_error: None,
                summary: None,
                summary_json: None,
                log_tail: None,
            })
            .await
            .unwrap();

        // Select through the real reconciliation query, not a hand-built row:
        // the force-pushed task must be *reachable* by the pass as well as
        // terminalizable by it.
        let selected = repo.list_reconcilable_pr_tasks().await.unwrap();
        let stale = selected
            .iter()
            .find(|t| t.id == task.id)
            .expect("a force-pushed pr_draft task with a merged PR must be reconcilable");
        assert_eq!(stale.ci_head_sha.as_deref(), Some(MERGED_HEAD));
        assert_eq!(
            stale.ci_github_head_sha.as_deref(),
            Some(STALE_ATTEMPT_HEAD)
        );
        assert_ne!(
            stale.ci_github_head_sha.as_deref(),
            Some(MERGE_COMMIT),
            "precondition: every stored head must disagree with the merge commit"
        );

        actor
            .terminalize_reconciled_merge(stale, pr_url, Some(MERGE_COMMIT))
            .await;

        let closed = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(
            closed.status, "closed",
            "a stale stored head SHA must not prevent a merged PR from terminalizing its task"
        );
        assert_eq!(closed.merge_commit_sha.as_deref(), Some(MERGE_COMMIT));

        cancel.cancel();
        db.pool().close().await;
    }

    /// Terminalizing twice must be a no-op the second time: the reconciler and
    /// the (now-fixed) poller loop can both observe the same merged PR, and the
    /// loser must not emit a second transition or a second audit row.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconciled_merge_is_idempotent() {
        let db = Database::open_in_memory().unwrap();
        let (actor, cancel, repo, epic_id) = reconcile_fixture(&db).await;

        let pr_url = "https://github.com/djinnos/djinn/pull/4242";
        let task = seed_task(&repo, &epic_id, "double-observed", "pr_draft", Some(pr_url)).await;

        actor
            .terminalize_reconciled_merge(&task, pr_url, Some("cafef00dcafef00d"))
            .await;
        let after_first = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(after_first.status, "closed");
        let activity_after_first = repo.list_activity(&task.id).await.unwrap().len();

        // Second pass, still holding the PRE-merge task row — exactly what the
        // reconciler holds when the poller closes the task between the query
        // and the terminalization.
        actor
            .terminalize_reconciled_merge(&task, pr_url, Some("cafef00dcafef00d"))
            .await;

        let after_second = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(after_second.status, "closed");
        assert_eq!(
            after_second.closed_at, after_first.closed_at,
            "a second reconciliation pass must not re-close an already-closed task"
        );
        assert_eq!(
            repo.list_activity(&task.id).await.unwrap().len(),
            activity_after_first,
            "a second reconciliation pass must not emit any audit row"
        );

        cancel.cancel();
        db.pool().close().await;
    }

    /// Negative control: a task the POLLER already terminalized (via the normal
    /// `pr_merge` path, no reconciliation reason) must be left completely alone
    /// — no second transition, no reconciliation audit row grafted onto it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn poller_terminalized_task_is_left_alone() {
        let db = Database::open_in_memory().unwrap();
        let (actor, cancel, repo, epic_id) = reconcile_fixture(&db).await;

        let pr_url = "https://github.com/djinnos/djinn/pull/4243";
        let task = seed_task(&repo, &epic_id, "poller-closed", "pr_draft", Some(pr_url)).await;
        repo.set_merge_commit_sha(&task.id, "beefbeefbeefbeef")
            .await
            .unwrap();
        repo.transition(
            &task.id,
            TransitionAction::PrMerge,
            "system",
            "pr_poller",
            None,
            None,
        )
        .await
        .unwrap();
        let baseline_activity = repo.list_activity(&task.id).await.unwrap().len();

        actor
            .terminalize_reconciled_merge(&task, pr_url, Some("beefbeefbeefbeef"))
            .await;

        let after = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(after.status, "closed");
        assert_eq!(
            repo.list_activity(&task.id).await.unwrap().len(),
            baseline_activity,
            "reconciling a task the poller already closed must be a pure no-op"
        );
        assert!(
            !repo
                .list_activity(&task.id)
                .await
                .unwrap()
                .iter()
                .any(|a| a.payload.contains("blind-spot merged-PR reconciliation")),
            "no reconciliation audit row may be grafted onto a normally-closed task"
        );

        cancel.cancel();
        db.pool().close().await;
    }

    // ── Merge terminalization via the state machine ─────────────────────────
    //
    // The blind-spot (non-poller-owned) route additionally needs the
    // `user_override → pr_review` hop, validated directly against the state
    // machine and merge bookkeeping below.
    #[tokio::test]
    async fn open_task_with_merged_pr_closes_with_merge_semantics_and_audit() {
        let db = Database::open_in_memory().unwrap();
        let bus = EventBus::noop();
        let epic_repo = EpicRepository::new(db.clone(), bus.clone());
        let epic = epic_repo
            .create("recon epic", "", "", "", "", None)
            .await
            .unwrap();
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
            primary_blocking_check: None,
            failure_annotations: None,
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
        repo.set_merge_commit_sha(&task.id, "cafef00d")
            .await
            .unwrap();
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
