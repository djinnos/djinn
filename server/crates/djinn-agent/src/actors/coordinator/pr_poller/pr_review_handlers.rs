use super::*;

impl CoordinatorActor {
    /// Attach PR review feedback to the task activity log, increment the
    /// review-round counter, log a visibility comment, and optionally escalate
    /// when `PR_REVIEW_ROUND_THRESHOLD` is exceeded.
    ///
    /// Called when the PR poller detects `CHANGES_REQUESTED` on a task.
    pub(in crate::actors::coordinator) async fn attach_pr_review_feedback(
        &mut self,
        task_id: &str,
        task_short_id: &str,
        feedback: PrReviewFeedback,
    ) {
        let task_repo = self.task_repo();

        // ── Count prior review cycles ─────────────────────────────────────────
        let prior_cycles = match task_repo
            .query_activity(ActivityQuery {
                task_id: Some(task_id.to_owned()),
                event_type: Some(PR_REVIEW_CYCLE_EVENT.to_string()),
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
                    task_id,
                    error = %e,
                    "PR poller: failed to count review cycles"
                );
                0
            }
        };

        let round = prior_cycles + 1;

        // ── Store feedback as activity log entry ──────────────────────────────
        let feedback_payload = serde_json::json!({
            "pull_number": feedback.pull_number,
            "pr_url": feedback.pr_url,
            "round": round,
            "change_request_count": feedback.change_request_reviews.len(),
            "inline_comment_count": feedback.inline_comments.len(),
            "change_request_reviews": feedback.change_request_reviews.iter().map(|r| {
                serde_json::json!({
                    "reviewer": r.user.as_ref().map(|u| u.login.as_str()).unwrap_or("unknown"),
                    "state": r.state,
                    "html_url": r.html_url,
                    "submitted_at": r.submitted_at,
                })
            }).collect::<Vec<_>>(),
            "inline_comments": feedback.inline_comments.iter().map(|c| {
                serde_json::json!({
                    "reviewer": c.user.as_ref().map(|u| u.login.as_str()).unwrap_or("unknown"),
                    "body": c.body,
                    "path": c.path,
                    "line": c.line,
                    "html_url": c.html_url,
                })
            }).collect::<Vec<_>>(),
        })
        .to_string();

        if let Err(e) = task_repo
            .log_activity(
                Some(task_id),
                "system",
                "system",
                PR_REVIEW_FEEDBACK_EVENT,
                &feedback_payload,
            )
            .await
        {
            tracing::warn!(
                task_id,
                error = %e,
                "PR poller: failed to store pr_review_feedback activity"
            );
        }

        // ── Record the review-cycle marker ────────────────────────────────────
        let cycle_payload = serde_json::json!({ "round": round }).to_string();
        if let Err(e) = task_repo
            .log_activity(
                Some(task_id),
                "coordinator",
                "system",
                PR_REVIEW_CYCLE_EVENT,
                &cycle_payload,
            )
            .await
        {
            tracing::warn!(
                task_id,
                error = %e,
                "PR poller: failed to store pr_review_cycle marker"
            );
        }

        // ── Log visibility comment for the review-fix cycle ───────────────────
        let reviewer_list = {
            let mut names: Vec<&str> = feedback
                .change_request_reviews
                .iter()
                .filter_map(|r| r.user.as_ref().map(|u| u.login.as_str()))
                .collect();
            names.dedup();
            if names.is_empty() {
                "reviewer(s)".to_string()
            } else {
                names.join(", ")
            }
        };
        let comment_body = format!(
            "**PR Review Round {round}**: Changes requested by {reviewer_list} on PR #{pull_number}. \
            Dispatching worker session with review feedback as context.",
            pull_number = feedback.pull_number
        );
        let comment_payload = serde_json::json!({ "body": comment_body }).to_string();
        if let Err(e) = task_repo
            .log_activity(
                Some(task_id),
                "coordinator",
                "system",
                "comment",
                &comment_payload,
            )
            .await
        {
            tracing::warn!(
                task_id,
                error = %e,
                "PR poller: failed to log review cycle comment"
            );
        }

        tracing::info!(
            task_id = %task_short_id,
            round,
            threshold = PR_REVIEW_ROUND_THRESHOLD,
            inline_comments = feedback.inline_comments.len(),
            "PR poller: review feedback attached (round {}/{})",
            round,
            PR_REVIEW_ROUND_THRESHOLD
        );

        // ── Escalate if threshold exceeded ────────────────────────────────────
        if round >= PR_REVIEW_ROUND_THRESHOLD {
            tracing::warn!(
                task_id = %task_short_id,
                round,
                threshold = PR_REVIEW_ROUND_THRESHOLD,
                "PR poller: review loop threshold reached — escalating to Lead/Architect"
            );

            let escalation_body = format!(
                "**PR Review Escalation**: Task has gone through {round} review rounds without approval \
                (threshold: {threshold}). Escalating to Lead/Architect for strategic review.\n\n\
                PR: {pr_url}",
                threshold = PR_REVIEW_ROUND_THRESHOLD,
                pr_url = feedback.pr_url
            );
            let escalation_payload = serde_json::json!({ "body": escalation_body }).to_string();
            if let Err(e) = task_repo
                .log_activity(
                    Some(task_id),
                    "coordinator",
                    "system",
                    "comment",
                    &escalation_payload,
                )
                .await
            {
                tracing::warn!(
                    task_id,
                    error = %e,
                    "PR poller: failed to log escalation comment"
                );
            }

            // Look up the task's project_id and escalate to Planner.
            // Per ADR-051 §8 the Planner is the escalation ceiling above Lead;
            // it can dispatch an Architect spike if the PR loop is structurally
            // wrong, or reshape the task itself.
            if let Ok(Some(task)) = self.task_repo().get(task_id).await {
                let reason = format!(
                    "PR review loop exceeded {PR_REVIEW_ROUND_THRESHOLD} rounds without approval. PR: {}",
                    feedback.pr_url
                );
                self.dispatch_planner_escalation(task_id, &reason, &task.project_id)
                    .await;
            }
        }
    }

    /// Re-request review from reviewers who previously submitted CHANGES_REQUESTED
    /// if the task has prior review feedback and no current outstanding changes request.
    ///
    /// This is called when the PR is still open, no CHANGES_REQUESTED is currently
    /// active (meaning the worker already pushed fixup commits), and the task has at
    /// least one prior `pr_review_feedback` activity entry.
    ///
    /// Non-fatal: logs warnings on any GitHub API failure.
    pub(in crate::actors::coordinator) async fn maybe_re_request_review(
        &mut self,
        task_id: &str,
        task_short_id: &str,
        gh_client: &GitHubApiClient,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) {
        let task_repo = self.task_repo();

        // Check if there is at least one prior review-feedback entry AND at least
        // one review-cycle entry. If no cycle entries exist, the worker has not
        // yet addressed any review, so there's nothing to re-request.
        let has_prior_cycles = match task_repo
            .query_activity(ActivityQuery {
                task_id: Some(task_id.to_owned()),
                event_type: Some(PR_REVIEW_CYCLE_EVENT.to_string()),
                actor_role: Some("system".to_string()),
                project_id: None,
                from_time: None,
                to_time: None,
                limit: 1,
                offset: 0,
            })
            .await
        {
            Ok(entries) => !entries.is_empty(),
            Err(_) => false,
        };

        if !has_prior_cycles {
            return;
        }

        // Check whether we have already re-requested review for the current
        // SHA (tracked via a `pr_re_review_requested` marker per SHA).
        // This avoids re-requesting on every 30-second tick.
        let current_sha_cache_key = format!("re_review:{task_id}");
        if self.pr_status_cache.contains_key(&current_sha_cache_key) {
            return; // Already re-requested for this SHA.
        }

        // Collect reviewer logins from the most recent pr_review_feedback entry.
        let reviewer_logins: Vec<String> = match task_repo
            .query_activity(ActivityQuery {
                task_id: Some(task_id.to_owned()),
                event_type: Some(PR_REVIEW_FEEDBACK_EVENT.to_string()),
                actor_role: Some("system".to_string()),
                project_id: None,
                from_time: None,
                to_time: None,
                limit: 1,
                offset: 0,
            })
            .await
        {
            Ok(entries) => entries
                .into_iter()
                .flat_map(|entry| {
                    let payload: serde_json::Value = serde_json::from_str(&entry.payload).ok()?;
                    let reviews = payload
                        .get("change_request_reviews")?
                        .as_array()?
                        .iter()
                        .filter_map(|r| {
                            r.get("reviewer")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_owned())
                        })
                        .collect::<Vec<_>>();
                    Some(reviews)
                })
                .flatten()
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect(),
            Err(_) => return,
        };

        if reviewer_logins.is_empty() {
            return;
        }

        tracing::info!(
            task_id = %task_short_id,
            pr = pull_number,
            reviewers = ?reviewer_logins,
            "PR poller: re-requesting review after fixup commits"
        );

        if let Err(e) = gh_client
            .re_request_review(owner, repo, pull_number, &reviewer_logins)
            .await
        {
            tracing::warn!(
                task_id = %task_short_id,
                pr = pull_number,
                error = %e,
                "PR poller: re-request review failed (non-fatal)"
            );
        } else {
            // Mark as done for this SHA so we don't re-request repeatedly.
            self.pr_status_cache
                .insert(current_sha_cache_key, "done".to_string());

            // Log a comment for visibility.
            let comment_body = format!(
                "**Re-requested review** from {} on PR #{pull_number} after fixup commits.",
                reviewer_logins.join(", ")
            );
            let comment_payload = serde_json::json!({ "body": comment_body }).to_string();
            let _ = task_repo
                .log_activity(
                    Some(task_id),
                    "coordinator",
                    "system",
                    "comment",
                    &comment_payload,
                )
                .await;
        }
    }

    pub(in crate::actors::coordinator) async fn apply_pr_transition(
        &self,
        task_id: &str,
        action: TransitionAction,
        reason: Option<&str>,
    ) {
        let task_repo = self.task_repo();
        let cleanup_action = action.clone();
        if let Err(e) = task_repo
            .transition(task_id, action, "system", "pr_poller", reason, None)
            .await
        {
            tracing::warn!(
                task_id,
                error = %e,
                "PR poller: failed to apply task transition"
            );
            return;
        }
        // Branch hygiene: once the task is closed (merged or force-closed via
        // any pr_poller path), delete the task branch on both the local mirror
        // and the GitHub remote.  Without this, stale `task/<short_id>` refs
        // pile up on every mirror clone and on GitHub.  Best-effort.
        if matches!(
            cleanup_action,
            TransitionAction::PrMerge | TransitionAction::ForceClose
        ) {
            let event_bus = crate::events::event_bus_for(&self.events_tx);
            crate::task_merge::cleanup_task_branches_post_close(
                task_id,
                &self.db,
                &event_bus,
                self.mirror.as_deref(),
            )
            .await;
        }
    }
}

/// Parse a GitHub PR URL into `(owner, repo, pull_number)`.
///
/// Handles URLs of the form `https://github.com/{owner}/{repo}/pull/{number}`.
pub(crate) fn parse_pr_url(url: &str) -> Option<(String, String, u64)> {
    let path = url.strip_prefix("https://github.com/")?;
    let mut parts = path.splitn(5, '/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    let segment = parts.next()?;
    if segment != "pull" {
        return None;
    }
    let number_str = parts.next()?;
    // Strip any trailing fragment/query.
    let number_str = number_str.split(&['?', '#'][..]).next()?;
    let number: u64 = number_str.parse().ok()?;
    Some((owner.to_string(), repo.to_string(), number))
}

/// True when `status` is a post-implementation, UNMERGED, racing PR state — the
/// shape of an epic sibling whose work is landing on main and can cause a
/// conflict. A merged/closed sibling is `closed`, which is deliberately
/// excluded (nothing to wait on; blocking on it would never release).
pub(in crate::actors::coordinator) fn is_racing_unmerged_status(status: &str) -> bool {
    matches!(status, "approved" | "pr_draft" | "pr_review")
}

/// Pick the single racing same-epic sibling to block `this_task_id` on, if
/// attribution is unambiguous.
///
/// Returns `Some(sibling_id)` ONLY when exactly one sibling (other than
/// `this_task_id`) is in a racing-unmerged state (see
/// [`is_racing_unmerged_status`]). Returns `None` for zero candidates (conflict
/// is against already-merged main — nothing to wait on) or more than one
/// (ambiguous — don't guess). Conservative by design: correctness over coverage.
pub(in crate::actors::coordinator) fn pick_conflict_blocker_sibling(
    this_task_id: &str,
    siblings: &[Task],
) -> Option<String> {
    let mut candidates = siblings
        .iter()
        .filter(|s| s.id != this_task_id && is_racing_unmerged_status(&s.status));
    let first = candidates.next()?;
    // More than one racing sibling → ambiguous attribution → fall back.
    if candidates.next().is_some() {
        return None;
    }
    Some(first.id.clone())
}

/// Collapse a PR's full review history into the *effective* merge-gating
/// decision, mirroring GitHub's own `reviewDecision`.
///
/// `GET /pulls/{n}/reviews` returns every review a reviewer ever submitted, and
/// a `CHANGES_REQUESTED` review's `state` stays `CHANGES_REQUESTED` forever —
/// pushing new commits, dismissing it, or even approving afterwards does NOT
/// rewrite that historical entry. The merge-gating decision therefore depends
/// only on each reviewer's *latest* standing review, never on whether any
/// historical review was CHANGES_REQUESTED.
///
/// Per author we take the most-recent review (by `submitted_at`, which is
/// RFC-3339 UTC so lexical ordering is chronological) whose state is one of
/// `APPROVED` / `CHANGES_REQUESTED` / `DISMISSED`. `COMMENTED` and `PENDING`
/// carry no standing and are ignored; `DISMISSED` clears a reviewer's prior
/// standing without adding a new one. Then:
///   - `changes_requested` is true iff some author's latest standing review is
///     `CHANGES_REQUESTED`.
///   - `has_approved` is true iff some author's latest standing review is
///     `APPROVED`.
///
/// Reviews with no author or no `submitted_at` are skipped (can't attribute or
/// order them). Returns `(changes_requested, has_approved)`.
pub(in crate::actors::coordinator) fn effective_review_decision(
    reviews: &[PrReview],
) -> (bool, bool) {
    use std::collections::HashMap;
    // author login -> (submitted_at, state) of their latest standing review.
    let mut latest: HashMap<&str, (&str, &str)> = HashMap::new();
    for r in reviews {
        let state = r.state.as_str();
        if !matches!(state, "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED") {
            continue; // COMMENTED / PENDING carry no merge-gating standing.
        }
        let Some(login) = r.user.as_ref().map(|u| u.login.as_str()) else {
            continue;
        };
        let Some(submitted) = r.submitted_at.as_deref() else {
            continue;
        };
        latest
            .entry(login)
            .and_modify(|cur| {
                if submitted > cur.0 {
                    *cur = (submitted, state);
                }
            })
            .or_insert((submitted, state));
    }
    let changes_requested = latest.values().any(|(_, s)| *s == "CHANGES_REQUESTED");
    let has_approved = latest.values().any(|(_, s)| *s == "APPROVED");
    (changes_requested, has_approved)
}
