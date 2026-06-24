use super::*;

impl CoordinatorActor {
    pub(crate) async fn resolve_unresolved_conversations(
        &self,
        gh_client: &GitHubApiClient,
        owner: &str,
        repo: &str,
        pull_number: u64,
        task_short_id: &str,
    ) -> Option<usize> {
        let ids = match gh_client
            .list_unresolved_review_thread_ids(owner, repo, pull_number)
            .await
        {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(
                    task_id = %task_short_id,
                    pr = pull_number,
                    error = %e,
                    "PR poller: failed to list unresolved review threads"
                );
                return None;
            }
        };

        let mut resolved = 0;
        for tid in &ids {
            match gh_client.resolve_review_thread(tid).await {
                Ok(()) => resolved += 1,
                Err(re) => tracing::warn!(
                    task_id = %task_short_id,
                    pr = pull_number,
                    thread = %tid,
                    error = %re,
                    "PR poller: resolve_review_thread failed"
                ),
            }
        }
        Some(resolved)
    }
}
pub(crate) fn is_conversation_resolution_block(
    err: &(impl crate::github_error_render::GithubWriteError + ?Sized),
) -> bool {
    crate::github_error_render::github_write_status_is(err, 405)
        && (crate::github_error_render::github_write_body_contains(
            err,
            "conversation must be resolved",
        ) || (crate::github_error_render::github_write_body_contains(
            err,
            "repository rule violations",
        ) && crate::github_error_render::github_write_body_contains(err, "conversation")))
}

/// Gate for proactively resolving review conversations on the GitHub-managed
/// **auto-merge** path (where no 405 error is ever surfaced to react to).
///
/// Fires only when:
///   * the PR is APPROVED — approval is the override signal that the
///     reviewer's inline comments are non-blocking (same policy as the direct
///     REST-merge 405 path), and
///   * GitHub reports `mergeStateStatus == "BLOCKED"` — mergeability is
///     otherwise satisfied (`CLEAN`/`UNSTABLE`/`BEHIND`/`DRAFT` all mean some
///     non-conversation condition is still in flight, so don't touch threads
///     yet), and
///   * we haven't already resolved conversations for this exact head SHA —
///     avoids re-querying GitHub's review threads every 30s tick when a
///     *different* protection rule (e.g. an outstanding CODEOWNERS review)
///     keeps the PR `BLOCKED` after the conversations are resolved.
///
/// Resolving is harmless when conversations weren't the real blocker: the
/// unresolved-thread list comes back empty and the resolve loop is a no-op.
pub(crate) fn should_auto_resolve_conversations(
    has_approved: bool,
    merge_state_status: Option<&str>,
    already_resolved_this_sha: bool,
) -> bool {
    has_approved && merge_state_status == Some("BLOCKED") && !already_resolved_this_sha
}
