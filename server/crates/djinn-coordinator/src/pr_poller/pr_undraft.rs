use super::*;

impl CoordinatorActor {
    /// Undraft a PR that has cleared every Djinn merge gate but is still a
    /// GitHub draft, so the subsequent merge/enqueue can proceed.
    ///
    /// The pr_review poller reaches the merge boundary only after the CI merge
    /// gate (and, where wired, the tripwire active-hold gate) has *allowed* the
    /// merge. A PR that is still a draft at that point cannot be merged or
    /// enqueued: `merge_pull_request` returns `405 "Pull Request is still a
    /// draft"` and `enqueuePullRequest` rejects it, so the poller 405-loops
    /// forever — attempt counter climbing, no self-heal — until a human runs
    /// `gh pr ready`. That is the ufsh / PR #1794 incident (2026-07-09).
    /// Marking the PR ready here closes the gap.
    ///
    /// Only invoked when the freshly-fetched draft flag is `true`
    /// ([`should_undraft_before_merge`]), so it never touches a PR that is held
    /// or failing (those `continue` out at the CI / tripwire gates upstream).
    ///
    /// Idempotent: `markPullRequestReadyForReview` errors on an already-ready
    /// PR, so if the fetched draft flag was stale this treats the "not a draft"
    /// rejection as success and proceeds.
    ///
    /// Returns `true` when the PR is now ready to merge (caller proceeds),
    /// `false` when the undraft genuinely failed (caller skips the merge this
    /// tick and retries next tick).
    pub(crate) async fn undraft_before_merge(
        &self,
        gh_client: &GitHubApiClient,
        task: &Task,
        pr_node_id: &str,
        pull_number: u64,
    ) -> bool {
        match gh_client.mark_pr_ready_for_review(pr_node_id).await {
            Ok(_) => {
                tracing::info!(
                    task_id = %task.short_id,
                    pr = pull_number,
                    "PR poller: undrafted mergeable draft PR before merge — all merge gates passed"
                );
                self.log_undraft_activity(task, pull_number).await;
                true
            }
            Err(e) if is_already_ready_error(&e) => {
                tracing::info!(
                    task_id = %task.short_id,
                    pr = pull_number,
                    "PR poller: PR already ready for review (stale draft flag) — proceeding to merge"
                );
                true
            }
            Err(e) => {
                tracing::warn!(
                    task_id = %task.short_id,
                    pr = pull_number,
                    error = %e,
                    "PR poller: failed to undraft mergeable draft PR — skipping merge this tick, will retry"
                );
                false
            }
        }
    }

    async fn log_undraft_activity(&self, task: &Task, pull_number: u64) {
        let payload = serde_json::json!({
            "pr": pull_number,
            "reason": "cleared merge gates while still a draft",
        })
        .to_string();
        if let Err(e) = self
            .task_repo()
            .log_activity(
                Some(&task.id),
                "coordinator",
                "system",
                "pr_undrafted_for_merge",
                &payload,
            )
            .await
        {
            tracing::warn!(
                task_id = %task.short_id,
                pr = pull_number,
                error = %e,
                "PR poller: failed to log pr_undrafted_for_merge activity"
            );
        }
    }
}

/// Whether the poller must undraft the PR before attempting the merge.
///
/// True only when GitHub reports the freshly-fetched PR as a draft
/// (`draft == Some(true)`). `Some(false)` (ready) and `None` (GitHub omitted
/// the field) both mean "leave draft state untouched".
pub(crate) fn should_undraft_before_merge(draft: Option<bool>) -> bool {
    draft == Some(true)
}

/// Detect the direct-merge `405 "Pull Request is still a draft"` rejection —
/// distinct from the merge-queue 405 ([`super::is_merge_queue_405`], body
/// "merge queue"). Used to attach a hint to the retry WARN when the pre-merge
/// undraft step evidently did not stick.
fn is_draft_merge_405(err: &(impl crate::github_error_render::GithubWriteError + ?Sized)) -> bool {
    crate::github_error_render::github_write_status_is(err, 405)
        && crate::github_error_render::github_write_body_contains(err, "still a draft")
}

/// Emit the retry WARN for a failed Djinn-initiated merge, attaching a hint
/// when the failure is the draft-405 that [`CoordinatorActor::undraft_before_merge`]
/// is meant to prevent — so an undraft step that silently fails is diagnosable
/// rather than an anonymous 405-loop.
pub(crate) fn warn_pr_merge_failed(
    task_short_id: &str,
    pull_number: u64,
    attempt: u32,
    err: &(impl crate::github_error_render::GithubWriteError + ?Sized),
) {
    let github_error = render_github_write_error("GitHub PR merge failed", err);
    let still_draft = is_draft_merge_405(err);
    tracing::warn!(
        task_id = %task_short_id,
        pr = pull_number,
        attempt,
        error = %github_error,
        still_draft,
        "PR poller: merge failed (will retry next tick){}",
        if still_draft {
            " — PR still a draft after undraft_before_merge; mark_pr_ready_for_review may be failing"
        } else {
            ""
        }
    );
}

/// Treat `markPullRequestReadyForReview` on an already-ready PR as success:
/// GitHub rejects it with a "not a draft" GraphQL error. Keeps the undraft
/// idempotent when the fetched draft flag was stale.
fn is_already_ready_error(err: &anyhow::Error) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("not a draft") || msg.contains("already ready")
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_provider::github_api::GitHubApiError;
    use reqwest::StatusCode;

    fn merge_405(body: &str) -> GitHubApiError {
        GitHubApiError::http(
            "PUT",
            "/repos/djinnos/djinn/pulls/7/merge".to_string(),
            StatusCode::METHOD_NOT_ALLOWED,
            body.to_string(),
        )
    }

    // Scenarios 1 & 4: undraft fires for a draft PR, is a no-op otherwise.
    // The call site guards on this before touching GitHub, so a ready PR
    // (`Some(false)`) or an unknown flag (`None`) leaves draft state alone.
    #[test]
    fn undraft_only_when_pr_is_draft() {
        assert!(should_undraft_before_merge(Some(true)));
        assert!(!should_undraft_before_merge(Some(false)));
        assert!(!should_undraft_before_merge(None));
    }

    // The retry-WARN hint fires on the real draft-405 body and stays distinct
    // from the merge-queue 405 (which routes to the enqueue path instead) and
    // from unrelated 405s.
    #[test]
    fn draft_405_is_detected_and_distinct() {
        let draft = merge_405(r#"{\"message\":\"Pull Request is still a draft\"}"#);
        assert!(is_draft_merge_405(&draft));

        let merge_queue = merge_405(r#"{\"message\":\"Pull Request is in the merge queue.\"}"#);
        assert!(!is_draft_merge_405(&merge_queue));

        let locked = merge_405(r#"{\"message\":\"locked\"}"#);
        assert!(!is_draft_merge_405(&locked));

        // Same body wording but a non-405 status must not match.
        let wrong_status = GitHubApiError::http(
            "PUT",
            "/repos/djinnos/djinn/pulls/7/merge".to_string(),
            StatusCode::UNPROCESSABLE_ENTITY,
            r#"{\"message\":\"Pull Request is still a draft\"}"#.to_string(),
        );
        assert!(!is_draft_merge_405(&wrong_status));
    }

    // Scenario: undraft on an already-ready PR (stale draft flag) is tolerated
    // as success so the merge proceeds; genuine failures are not.
    #[test]
    fn already_ready_error_is_tolerated_but_real_failures_are_not() {
        assert!(is_already_ready_error(&anyhow::anyhow!(
            "mark_pr_ready_for_review GraphQL error: Pull request is not a draft"
        )));
        assert!(is_already_ready_error(&anyhow::anyhow!(
            "PR is already ready for review"
        )));
        assert!(!is_already_ready_error(&anyhow::anyhow!(
            "mark_pr_ready_for_review failed (502): upstream error"
        )));
    }
}
