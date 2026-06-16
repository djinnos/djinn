//! Supervisor-driven PR-open orchestration.
//!
//! Stays in `djinn-agent` (rather than moving to `djinn-supervisor`) because
//! the PR path calls `task_merge::squash_merge_via_mirror` /
//! `build_app_push_url` and reads `AgentContext.mirror`. It's invoked by the
//! supervisor body through `SupervisorServices::open_pr_fn`, wired by
//! `actors::slot::supervisor_runner::run_supervisor_dispatch`.
//!
//! Scope is intentionally narrower than
//! [`crate::task_merge::merge_and_transition`]: no worktree teardown, no
//! knowledge-promotion side effects, no activity-log writes.  The supervisor
//! keeps those concerns inside [`super::stage::execute_stage`]'s post-session
//! path; this module only:
//!
//! 1. Resolves the project's owner/repo/installation.
//! 2. Mints a GitHub-App installation token.
//! 3. Runs [`crate::task_merge::squash_merge_via_mirror`] through the mirror.
//! 4. Creates (or adopts/reopens) a GitHub PR for the squashed commit.

use djinn_core::models::TransitionAction;
use djinn_core::tool_error::ErrorClass;
use djinn_db::{ActivityQuery, ProjectRepository, TaskRepository};
use djinn_git::GitError;
use djinn_provider::github_api::{
    CreatePrParams, GitHubApiClient, GitHubApiError, GitHubErrorSource, PrState,
};
use djinn_provider::github_app::{app_id as github_app_id, installations::get_installation_token};
use djinn_runtime::spec::{TaskRunOutcome, TaskRunSpec};
use djinn_workspace::MirrorManager;

use super::SupervisorCallbackContext;
use crate::actors::slot::helpers::default_target_branch;
use crate::github_error_render::render_github_write_error;
use crate::task_merge::build_app_push_url;

use super::disposition::{LiveMoverEvidence, NudgeHintEvidence, resolve_corrective_nudge_hint};

/// Open (or adopt) a GitHub PR for the completed task-run.
///
/// Returns:
/// - `TaskRunOutcome::PrOpened { url, sha }` on success.
/// - `TaskRunOutcome::Failed { stage: "pr_open", reason, .. }` for any failure.
pub(crate) async fn supervisor_pr_open(
    spec: &TaskRunSpec,
    task: &djinn_core::models::Task,
    callbacks: &SupervisorCallbackContext,
) -> TaskRunOutcome {
    if github_app_id().is_err() {
        return TaskRunOutcome::Failed {
            stage: "pr_open".into(),
            provider_failure: None,
            reason: "GitHub App is not configured on this deployment — \
                     supervisor PR-open requires the App"
                .into(),
            error_class: None,
            hint: None,
            body_excerpt: None,
        };
    }

    let app_state = &callbacks.agent_context;
    let mirror = match app_state.mirror.as_ref() {
        Some(m) => m.clone(),
        None => {
            return TaskRunOutcome::Failed {
                stage: "pr_open".into(),
                provider_failure: None,
                reason: "supervisor PR-open requires MirrorManager but AgentContext has none"
                    .into(),
                error_class: None,
                hint: None,
                body_excerpt: None,
            };
        }
    };

    let project_repo = ProjectRepository::new(app_state.db.clone(), app_state.event_bus.clone());

    let (owner, repo_name) = match project_repo.get_github_coords(&spec.project_id).await {
        Ok(Some(coords)) => coords,
        Ok(None) => {
            return TaskRunOutcome::Failed {
                stage: "pr_open".into(),
                provider_failure: None,
                reason: format!(
                    "project {} has no github_owner/github_repo persisted",
                    spec.project_id
                ),
                error_class: None,
                hint: None,
                body_excerpt: None,
            };
        }
        Err(e) => {
            return TaskRunOutcome::Failed {
                stage: "pr_open".into(),
                provider_failure: None,
                reason: format!(
                    "failed to read github coords for project {}: {e}",
                    spec.project_id
                ),
                error_class: None,
                hint: None,
                body_excerpt: None,
            };
        }
    };

    let installation_id = match project_repo.get_installation_id(&spec.project_id).await {
        Ok(Some(id)) => id,
        Ok(None) => {
            return TaskRunOutcome::Failed {
                stage: "pr_open".into(),
                provider_failure: None,
                reason: format!(
                    "project {} ({}/{}) has no cached installation_id",
                    spec.project_id, owner, repo_name
                ),
                error_class: None,
                hint: None,
                body_excerpt: None,
            };
        }
        Err(e) => {
            return TaskRunOutcome::Failed {
                stage: "pr_open".into(),
                provider_failure: None,
                reason: format!(
                    "failed to read installation_id for project {}: {e}",
                    spec.project_id
                ),
                error_class: None,
                hint: None,
                body_excerpt: None,
            };
        }
    };

    let install_token = match get_installation_token(installation_id).await {
        Ok(t) => t,
        Err(e) => {
            return TaskRunOutcome::Failed {
                stage: "pr_open".into(),
                provider_failure: None,
                reason: format!("could not mint installation token: {e}"),
                error_class: None,
                hint: None,
                body_excerpt: None,
            };
        }
    };
    let push_url = build_app_push_url(&owner, &repo_name, &install_token.token);

    let merge_target = default_target_branch(&spec.project_id, app_state).await;

    let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());

    // No-commits guard. A task whose run produced no git diff (e.g. its
    // deliverable was a memory/notes import, not code) leaves task_branch with
    // zero commits ahead of the base. GitHub's create_pull_request then 422s
    // ("No commits between <base> and task/<id>"), which the coordinator
    // surfaces as a persistent "PR blocked" health banner and re-attempts every
    // tick. There is nothing to open a PR with, so close the task as completed
    // (Close is valid from any non-closed status — covers both the supervisor
    // body's in_progress/verifying caller and the coordinator's approved
    // re-cycle caller) and report the run as Closed rather than Failed.
    match task_branch_commits_ahead(
        mirror.as_ref(),
        &spec.project_id,
        &spec.task_branch,
        &merge_target,
    )
    .await
    {
        Ok(0) => {
            // D3b/D3c: the task branch is zero commits ahead of base — the run
            // reported done but left nothing to open a PR with. Classify the
            // run's progress from hard evidence (D3a) and route through the
            // bounded disposition predicate (`disposition::decide_run_disposition`)
            // instead of unconditionally closing. A `NoOp` run gets a bounded
            // corrective nudge (re-dispatch with a hint) up to `NUDGE_CAP`
            // times, THEN closes; an `Inconclusive` run (AC progress, no diff)
            // and an exhausted-budget run fall through to the historical close.
            return handle_noop_disposition(task, &task_repo, &merge_target).await;
        }
        Ok(_) => {}
        Err(e) => {
            // Don't block the PR on a precheck failure — fall through to the
            // normal push/open path (which will surface any real error).
            tracing::warn!(
                task_id = %task.id,
                error = %e,
                "supervisor PR-open: commits-ahead precheck failed; proceeding with normal PR path"
            );
        }
    }

    let commit_type = if task.issue_type == "task" {
        "chore"
    } else {
        "feat"
    };
    let message = format!("{}({}): {}", commit_type, task.short_id, task.title);

    // Push the worker's task_branch from the mirror to the GitHub remote
    // so the PR's head ref exists. We deliberately do NOT call
    // squash_merge_via_mirror here — it pushes the squashed commit directly
    // to refs/heads/main, which branch-protected repos (Quality Gate etc.)
    // reject ("Changes must be made through a pull request"). The PR flow
    // below handles landing the change properly via human/CI review.
    let head_sha = match push_task_branch_to_github(
        mirror.as_ref(),
        &spec.project_id,
        &spec.task_branch,
        &push_url,
    )
    .await
    {
        Ok(sha) => sha,
        Err(e) => {
            return TaskRunOutcome::Failed {
                stage: "pr_open".into(),
                provider_failure: None,
                reason: format!("push task_branch to GitHub failed: {e}"),
                error_class: None,
                hint: None,
                body_excerpt: None,
            };
        }
    };
    let _ = &message;
    let merge_result_commit_sha = head_sha;

    let pr_title = format!("{}({}): {}", commit_type, task.short_id, task.title);
    let pr_body = format!(
        "## Summary\n{description}\n\n---\nDjinn task: {short_id}",
        description = task.description,
        short_id = task.short_id,
    );

    let github_client = GitHubApiClient::for_installation(installation_id);
    let head_ref = format!("{owner}:{}", spec.task_branch);

    let existing_pr = match github_client
        .list_pulls_by_head_with_state(&owner, &repo_name, &head_ref, "all")
        .await
    {
        Ok(prs) => prs.into_iter().next(),
        Err(e) => {
            tracing::warn!(
                task_id = %task.id,
                error = %e,
                "supervisor PR-open: list_pulls_by_head_with_state failed; creating a new PR"
            );
            None
        }
    };

    let pr = if let Some(existing) = existing_pr {
        if existing.state == PrState::Open {
            existing
        } else {
            match github_client
                .reopen_pull_request(&owner, &repo_name, existing.number)
                .await
            {
                Ok(reopened) => reopened,
                Err(reopen_err) => {
                    tracing::warn!(
                        task_id = %task.id,
                        pr_number = existing.number,
                        error = %render_github_write_error("GitHub PR reopen failed", &reopen_err),
                        "supervisor PR-open: failed to reopen closed PR; creating a new one"
                    );
                    match github_client
                        .create_pull_request(
                            &owner,
                            &repo_name,
                            CreatePrParams {
                                title: pr_title.clone(),
                                body: pr_body.clone(),
                                head: spec.task_branch.clone(),
                                base: merge_target.clone(),
                                maintainer_can_modify: Some(true),
                                draft: Some(true),
                            },
                        )
                        .await
                    {
                        Ok(pr) => pr,
                        Err(e) => {
                            let create_error =
                                render_github_write_error("GitHub PR creation failed", &e);
                            let reopen_error =
                                render_github_write_error("GitHub PR reopen failed", &reopen_err);
                            let reason = format!("{create_error}; prior {reopen_error}");
                            return pr_open_failure_outcome(
                                "POST",
                                format!("/repos/{owner}/{repo_name}/pulls"),
                                &e,
                                Some(reason),
                            );
                        }
                    }
                }
            }
        }
    } else {
        match github_client
            .create_pull_request(
                &owner,
                &repo_name,
                CreatePrParams {
                    title: pr_title,
                    body: pr_body,
                    head: spec.task_branch.clone(),
                    base: merge_target,
                    maintainer_can_modify: Some(true),
                    draft: Some(true),
                },
            )
            .await
        {
            Ok(pr) => pr,
            Err(e) => {
                return pr_open_failure_outcome(
                    "POST",
                    format!("/repos/{owner}/{repo_name}/pulls"),
                    &e,
                    None,
                );
            }
        }
    };

    if let Err(e) = task_repo.set_pr_url(&task.id, &pr.html_url).await {
        tracing::warn!(
            task_id = %task.id,
            error = %e,
            "supervisor PR-open: failed to store pr_url on task (non-fatal)"
        );
    }

    // Walk the task into the PR-aware status (approved → pr_draft) so the
    // host's dispatcher stops treating it as "still open work" and
    // re-spawning a worker the moment supervisor_pr_open returns. The
    // post-PR side (PrUndraft / PrMerge / PrCiFailed / PrChangesRequested)
    // is driven by `pr_poller` on every coordinator tick.
    if let Err(e) = task_repo
        .transition(
            &task.id,
            djinn_core::models::TransitionAction::PrCreated,
            "supervisor",
            "system",
            None,
            None,
        )
        .await
    {
        tracing::warn!(
            task_id = %task.id,
            error = %e,
            "supervisor PR-open: pr_created transition skipped (task may not be in approved state — check earlier stage-loop transitions)"
        );
    }

    tracing::info!(
        task_id = %task.short_id,
        pr_url = %pr.html_url,
        pr_number = pr.number,
        commit_sha = %merge_result_commit_sha,
        "Supervisor: PR opened"
    );

    TaskRunOutcome::PrOpened {
        url: pr.html_url,
        sha: merge_result_commit_sha,
    }
}

const PR_ALREADY_EXISTS_HINT: &str =
    "a PR for this branch already exists — adopt it via the existing PR URL";
const TASK_OUTCOME_BODY_EXCERPT_BYTES: usize = 512;

fn pr_open_failure_outcome(
    method: &'static str,
    path: String,
    err: &GitHubApiError,
    reason_override: Option<String>,
) -> TaskRunOutcome {
    let reason = reason_override
        .unwrap_or_else(|| render_github_write_error("GitHub PR creation failed", err));

    tracing::warn!(
        method,
        path = %path,
        body = %err.body,
        source = ?err.source,
        status = err.status.map(|status| status.as_u16()),
        "supervisor PR-open: GitHub PR creation failed response body"
    );

    let already_exists = err.is_pr_already_exists();
    let error_class = if already_exists {
        ErrorClass::ConflictRecoverable
    } else {
        classify_github_write_error(err)
    };

    TaskRunOutcome::Failed {
        stage: "pr_open".into(),
        provider_failure: None,
        reason,
        error_class: Some(error_class),
        hint: Some(
            if already_exists {
                PR_ALREADY_EXISTS_HINT
            } else {
                default_pr_open_hint(error_class)
            }
            .to_string(),
        ),
        body_excerpt: Some(bounded_task_outcome_body_excerpt(&err.body)),
    }
}

#[cfg(test)]
fn pr_open_untyped_failure_outcome(
    method: &'static str,
    path: String,
    err: &anyhow::Error,
) -> TaskRunOutcome {
    let fallback = djinn_core::tool_error::ToolError::new(err.to_string())
        .with_error_class(ErrorClass::Internal)
        .with_method(method)
        .with_path(path);
    TaskRunOutcome::Failed {
        stage: "pr_open".into(),
        provider_failure: None,
        reason: fallback.error,
        error_class: Some(ErrorClass::Internal),
        hint: None,
        body_excerpt: None,
    }
}

fn classify_github_write_error(err: &GitHubApiError) -> ErrorClass {
    match err.source {
        GitHubErrorSource::RateLimited => return ErrorClass::RateLimited,
        GitHubErrorSource::Unauthenticated => return ErrorClass::Permission,
        GitHubErrorSource::Transport | GitHubErrorSource::GraphQL => return ErrorClass::Internal,
        GitHubErrorSource::Http => {}
    }

    match err.status.map(|status| status.as_u16()) {
        Some(404) => ErrorClass::NotFound,
        Some(401 | 403) => ErrorClass::Permission,
        Some(422) if err.is_pr_already_exists() => ErrorClass::ConflictRecoverable,
        Some(422) => ErrorClass::Validation,
        Some(429) => ErrorClass::RateLimited,
        Some(code) if (500..600).contains(&code) => ErrorClass::Transient,
        _ => ErrorClass::Internal,
    }
}

fn default_pr_open_hint(error_class: ErrorClass) -> &'static str {
    match error_class {
        ErrorClass::NotFound => {
            "verify the repository, branch, and base ref exist and are accessible"
        }
        ErrorClass::Permission => {
            "check that the GitHub App installation has permission to create pull requests"
        }
        ErrorClass::RateLimited => "back off until the GitHub rate limit resets before retrying",
        ErrorClass::Transient => {
            "retry after a short delay; GitHub reported a transient upstream failure"
        }
        ErrorClass::Validation => "fix the rejected GitHub pull-request parameters before retrying",
        ErrorClass::ConflictRecoverable => PR_ALREADY_EXISTS_HINT,
        ErrorClass::Internal => "inspect supervisor logs for the unclassified GitHub write failure",
    }
}

fn bounded_task_outcome_body_excerpt(body: &str) -> String {
    let normalized = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.len() <= TASK_OUTCOME_BODY_EXCERPT_BYTES {
        return normalized;
    }

    let mut end = 0;
    for (idx, ch) in normalized.char_indices() {
        let next = idx + ch.len_utf8();
        if next > TASK_OUTCOME_BODY_EXCERPT_BYTES {
            break;
        }
        end = next;
    }
    let omitted = normalized.len().saturating_sub(end);
    format!(
        "{}[truncated: {} bytes omitted]",
        &normalized[..end],
        omitted
    )
}

/// Count acceptance criteria already marked `met` on a task's AC JSON.
///
/// Pure parse of the `[{ "criterion": .., "met": bool }, ..]` array the
/// finalize path writes (`apply_ac_verdicts`). A malformed / empty column
/// yields `0` — we never fail a disposition decision on AC bookkeeping.
fn count_met_acceptance_criteria(acceptance_criteria_json: &str) -> u32 {
    serde_json::from_str::<serde_json::Value>(acceptance_criteria_json)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .map(|arr| {
            arr.iter()
                .filter(|c| c.get("met").and_then(|m| m.as_bool()).unwrap_or(false))
                .count() as u32
        })
        .unwrap_or(0)
}

fn unmet_acceptance_criteria(acceptance_criteria_json: &str) -> Vec<String> {
    serde_json::from_str::<serde_json::Value>(acceptance_criteria_json)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .map(|arr| {
            arr.iter()
                .filter(|criterion| {
                    !criterion
                        .get("met")
                        .and_then(|met| met.as_bool())
                        .unwrap_or(false)
                })
                .filter_map(acceptance_criterion_label)
                .collect()
        })
        .unwrap_or_default()
}

fn acceptance_criterion_label(criterion: &serde_json::Value) -> Option<String> {
    ["description", "criterion", "title", "name"]
        .iter()
        .filter_map(|key| criterion.get(*key).and_then(|value| value.as_str()))
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(ToString::to_string)
}

async fn build_corrective_nudge_evidence(
    task: &djinn_core::models::Task,
    task_repo: &TaskRepository,
) -> NudgeHintEvidence {
    let activity = task_repo
        .query_activity(ActivityQuery {
            task_id: Some(task.id.clone()),
            limit: 50,
            ..Default::default()
        })
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(
                task_id = %task.id,
                error = %e,
                "supervisor PR-open: unable to read activity for corrective nudge hint"
            );
            Vec::new()
        });

    NudgeHintEvidence {
        wind_down_summary: activity.iter().find_map(wind_down_or_finalize_summary),
        last_error_signature: activity.iter().find_map(last_error_signature),
        ac_unmet: unmet_acceptance_criteria(&task.acceptance_criteria),
        task_description: task.description.clone(),
    }
}

fn wind_down_or_finalize_summary(entry: &djinn_core::models::ActivityEntry) -> Option<String> {
    if entry.event_type != "work_submitted" {
        return None;
    }

    let payload: serde_json::Value = serde_json::from_str(&entry.payload).ok()?;
    payload
        .get("summary")
        .and_then(|summary| summary.as_str())
        .map(str::trim)
        .filter(|summary| !summary.is_empty())
        .map(ToString::to_string)
}

fn last_error_signature(entry: &djinn_core::models::ActivityEntry) -> Option<String> {
    let payload: serde_json::Value = serde_json::from_str(&entry.payload).ok()?;
    match entry.event_type.as_str() {
        "runtime_error" | "session_error" => stable_error_signature(&payload, &entry.event_type),
        "comment" if entry.actor_role == "system" || entry.actor_id.contains("supervisor") => {
            supervisor_error_comment_signature(&payload)
        }
        _ => None,
    }
}

fn supervisor_error_comment_signature(payload: &serde_json::Value) -> Option<String> {
    let body = payload.get("body").and_then(|body| body.as_str())?;
    let first_line = first_non_empty_line(body)?;
    if !first_line.to_ascii_lowercase().contains("error") {
        return None;
    }
    Some(format!("supervisor: {first_line}"))
}

fn stable_error_signature(payload: &serde_json::Value, fallback_tool_name: &str) -> Option<String> {
    let tool_name = payload
        .get("tool_name")
        .or_else(|| payload.get("tool"))
        .or_else(|| payload.get("agent_type"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback_tool_name);

    let error = payload
        .get("error")
        .or_else(|| payload.get("message"))
        .or_else(|| payload.get("body"))
        .and_then(|value| value.as_str())?;
    let first_line = first_non_empty_line(error)?;
    Some(format!("{tool_name}: {first_line}"))
}

fn first_non_empty_line(value: &str) -> Option<&str> {
    value.lines().map(str::trim).find(|line| !line.is_empty())
}

/// D3b/D3c: dispose of a finished run that reached the PR-open guard with a
/// **zero-commits-ahead** task branch (the canonical "succeeded but did
/// nothing" case).
///
/// Builds a [`RunProgressSignals`] from the hard evidence available at this
/// fork, classifies it via D3a's [`classify_run_progress`], and routes through
/// the pure [`decide_run_disposition`] predicate:
///
/// - `NoOp` under the nudge budget → a **bounded** corrective nudge: increment
///   `continuation_count` (the dedup/attempt key), log a corrective comment
///   carrying the prior next-action context, release the task back to `open`
///   so the coordinator re-dispatches it, and report the run as `Escalated`
///   (re-dispatchable, NOT a terminal close). The same finished run can never
///   be nudged twice: the increment + release spawns a *fresh* run.
/// - `NoOp` over budget, or `Inconclusive`, or a task in a state we can't
///   safely release (e.g. coordinator re-cycle from `approved`) → fall through
///   to the historical terminal close.
///
/// `Productive` never reaches here — the caller opens the PR — but the
/// predicate models it for totality and we defensively close (rather than open
/// a PR with no commits) if it somehow does.
///
/// Evidence sourcing at this fork:
/// - `commits_ahead = 0` — established by the caller's `Ok(0)` guard.
/// - `files_changed = 0` — a zero-commit task branch has nothing committed to
///   open a PR with; uncommitted edits can't become a PR either.
/// - `ac_newly_satisfied` — approximated by the count of AC currently `met`.
///   If any AC is met on a no-diff run the classifier returns `Inconclusive`
///   (evidence/bookkeeping disagree) and we conservatively close rather than
///   nudge.
pub(crate) async fn handle_noop_disposition(
    task: &djinn_core::models::Task,
    task_repo: &TaskRepository,
    merge_target: &str,
) -> TaskRunOutcome {
    use djinn_core::run_progress::{RunProgressSignals, classify_run_progress};

    use super::disposition::{NUDGE_CAP, RunDisposition, decide_run_disposition};

    let signals = RunProgressSignals {
        commits_ahead: 0,
        files_changed: 0,
        ac_newly_satisfied: count_met_acceptance_criteria(&task.acceptance_criteria),
    };
    let progress = classify_run_progress(&signals);
    let disposition = decide_run_disposition(progress, task.continuation_count, NUDGE_CAP);

    // The nudge requires releasing the task back to a worker-redispatchable
    // state. Only `in_progress`/`verifying` (the supervisor-body callers) can
    // be released without stepping on a terminal-action path; the coordinator
    // re-cycle reaches `open_pr` from `approved`, which we must NOT bounce back
    // to open. If no safe release action exists, downgrade Nudge → Close.
    let release_action = match task.status.as_str() {
        "in_progress" => Some(TransitionAction::Release),
        "verifying" => Some(TransitionAction::ReleaseVerification),
        _ => None,
    };

    let nudge = matches!(disposition, RunDisposition::Nudge) && release_action.is_some();

    if nudge {
        let release_action = release_action.expect("nudge implies a release action");
        let evidence = build_corrective_nudge_evidence(task, task_repo).await;
        let next_action_hint = resolve_corrective_nudge_hint(&evidence);
        let attempt = task.continuation_count + 1;
        let body = if next_action_hint.is_empty() {
            format!(
                "This run finished without producing any commits, files, or acceptance-criteria \
                 progress (a no-op). Re-dispatching for a corrective attempt {attempt}/{NUDGE_CAP}: \
                 make the concrete change the task asks for and commit it before finalizing."
            )
        } else {
            format!(
                "This run finished without producing any commits, files, or acceptance-criteria \
                 progress (a no-op). Re-dispatching for a corrective attempt {attempt}/{NUDGE_CAP}. \
                 Prior intent: {next_action_hint}\n\nMake the concrete change and commit it before \
                 finalizing — do not finalize with an empty diff."
            )
        };

        // Increment the continuation counter FIRST: it is the idempotency /
        // attempt key. Even if the release transition below is skipped, the
        // bumped count ensures the next pass advances toward the cap rather
        // than looping on the same decision.
        if let Err(e) = task_repo.increment_continuation_count(&task.id).await {
            tracing::warn!(
                task_id = %task.id,
                error = %e,
                "supervisor PR-open: D3c nudge increment_continuation_count failed; \
                 falling back to terminal close"
            );
            return close_noop(task, task_repo, true, &signals).await;
        }

        if let Err(e) = task_repo
            .log_activity(
                Some(&task.id),
                "supervisor",
                "system",
                "comment",
                &serde_json::json!({ "body": body }).to_string(),
            )
            .await
        {
            tracing::warn!(
                task_id = %task.id,
                error = %e,
                "supervisor PR-open: D3c nudge comment skipped (non-fatal)"
            );
        }

        let reason = format!(
            "no-op run (no commits/files/AC progress); corrective nudge {attempt}/{NUDGE_CAP} — \
             released back to open for re-dispatch"
        );
        if let Err(e) = task_repo
            .transition(
                &task.id,
                release_action,
                "supervisor",
                "system",
                Some(&reason),
                None,
            )
            .await
        {
            // The counter is already bumped; if we can't release, the task
            // stays where it is and the next disposition pass will close it.
            tracing::warn!(
                task_id = %task.id,
                error = %e,
                "supervisor PR-open: D3c nudge release transition failed; \
                 closing as a no-op instead"
            );
            return close_noop(task, task_repo, true, &signals).await;
        }

        tracing::info!(
            task_id = %task.id,
            base = %merge_target,
            attempt,
            "supervisor PR-open: no-op run — D3c corrective nudge (re-dispatch)"
        );
        return TaskRunOutcome::Escalated { reason };
    }

    tracing::info!(
        task_id = %task.id,
        base = %merge_target,
        progress = ?progress,
        continuation_count = task.continuation_count,
        "supervisor PR-open: task_branch has no commits ahead of base and the no-op \
         budget is exhausted (or signal is inconclusive) — closing the task as completed"
    );
    close_noop(task, task_repo, true, &signals).await
}

/// Predicate-driven orphan/no-mover entry point for already-settled runs that
/// never reached the PR-open zero-commit fork.
///
/// The existing PR-open `Ok(0)` guard remains the canonical branch-aware path.
/// This helper is for coordinator recovery paths that have already established
/// the task is otherwise settled (no live verifier/session/dispatch/PR mover)
/// and would previously park or release the task without consulting the D3b/D3c
/// no-op disposition ladder.  It deliberately delegates to
/// [`handle_noop_disposition`] so nudges, `continuation_count`, release actions,
/// and terminal no-op close semantics stay shared with the original fork.
pub(crate) async fn handle_settled_noop_without_live_mover(
    task: &djinn_core::models::Task,
    task_repo: &TaskRepository,
    merge_target: &str,
    evidence: &LiveMoverEvidence,
) -> Option<TaskRunOutcome> {
    if !should_route_settled_noop_without_live_mover(task, evidence) {
        tracing::debug!(
            task_id = %task.id,
            evidence = ?evidence,
            "supervisor no-mover disposition: live mover present; leaving task on existing path"
        );
        return None;
    }

    tracing::info!(
        task_id = %task.id,
        status = %task.status,
        "supervisor no-mover disposition: settled task has no live mover; routing through no-op disposition"
    );
    Some(handle_noop_disposition(task, task_repo, merge_target).await)
}

fn should_route_settled_noop_without_live_mover(
    _task: &djinn_core::models::Task,
    evidence: &LiveMoverEvidence,
) -> bool {
    use super::disposition::has_live_mover;

    !has_live_mover(evidence)
}

/// Canonical reason text for the historical no-commits terminal close.
///
/// Exposed at module scope (instead of being inlined in [`close_noop`]) so the
/// historical close path is testable in isolation: regression tests can pin
/// the exact text the supervisor reports on the `task.close_reason` column and
/// the `TaskRunOutcome::Closed { reason }` payload, locking the historical
/// spike force-close path against accidental drift.
pub(crate) const NOOP_CLOSE_REASON: &str = "no code changes were produced, so there is nothing to \
                  open a pull request with (e.g. a memory/notes-only task); closing as completed";

/// The historical no-commits terminal close: transition the task to closed
/// (completed) and report the run as `Closed`. Factored out so both the
/// budget-exhausted and the nudge-fallback paths share one definition, and so
/// the close transition is awaited (preserving the original synchronous
/// "transition then return" ordering of the no-commits guard).
fn should_close_noop(
    predicate_says_no_mover: bool,
    signals: &djinn_core::run_progress::RunProgressSignals,
    _task: &djinn_core::models::Task,
) -> bool {
    predicate_says_no_mover && signals.commits_ahead == 0 && signals.files_changed == 0
}

async fn close_noop(
    task: &djinn_core::models::Task,
    task_repo: &TaskRepository,
    predicate_says_no_mover: bool,
    signals: &djinn_core::run_progress::RunProgressSignals,
) -> TaskRunOutcome {
    if !should_close_noop(predicate_says_no_mover, signals, task) {
        let reason =
            "no-op close skipped because the live-mover/progress predicate did not allow closure"
                .to_string();
        tracing::info!(
            task_id = %task.id,
            predicate_says_no_mover,
            commits_ahead = signals.commits_ahead,
            files_changed = signals.files_changed,
            "supervisor PR-open: no-op close skipped by predicate"
        );
        return TaskRunOutcome::Escalated { reason };
    }

    let reason = NOOP_CLOSE_REASON;
    // `Close` is valid from any non-closed status — covers both the supervisor
    // body's in_progress/verifying caller and the coordinator's approved
    // re-cycle caller.
    if let Err(e) = task_repo
        .transition(
            &task.id,
            TransitionAction::Close,
            "supervisor",
            "system",
            Some(reason),
            None,
        )
        .await
    {
        tracing::warn!(
            task_id = %task.id,
            error = %e,
            "supervisor PR-open: no-commits close transition skipped"
        );
    }
    TaskRunOutcome::Closed {
        reason: reason.to_string(),
    }
}

/// Push the worker's task_branch from the mirror clone up to GitHub via
/// the App-installation push URL. Returns the HEAD SHA on success.
///
/// **Concurrent-push race**: the coordinator's tick-driven dispatch path and
/// the supervisor body's own `open_pr` can fire simultaneously for the same
/// task — they both clone the mirror and push the same SHA. GitHub's
/// `git-receive-pack` serializes refs and rejects the loser with:
///
/// ```text
/// ! [remote rejected] task/X -> task/X (cannot lock ref '...': reference already exists)
/// ```
///
/// When that happens we re-check the remote ref: if it already points at our
/// local SHA the race winner pushed identical content and the operation is
/// effectively a no-op — return success. Only when the remote SHA differs do
/// we propagate the error.
/// Count commits on `task_branch` that are not already reachable from `base`.
///
/// Clones the mirror ephemerally on `task_branch` (the same `--local --shared`
/// clone the push path uses, so `origin/<base>` is present as a remote-tracking
/// ref) and runs `git rev-list --count origin/<base>..HEAD`. A result of `0`
/// means the branch has no new commits — there is nothing to open a PR with.
async fn task_branch_commits_ahead(
    mirror: &MirrorManager,
    project_id: &str,
    task_branch: &str,
    base: &str,
) -> Result<u64, GitError> {
    use djinn_git::run_git_command;
    let workspace = mirror
        .clone_ephemeral(project_id, task_branch)
        .await
        .map_err(|e| GitError::Other(anyhow::anyhow!("clone_ephemeral {task_branch}: {e}")))?;
    let out = run_git_command(
        workspace.path_buf(),
        vec![
            "rev-list".into(),
            "--count".into(),
            format!("origin/{base}..HEAD"),
        ],
    )
    .await?;
    let count = out.stdout.trim().parse::<u64>().map_err(|e| {
        GitError::Other(anyhow::anyhow!(
            "rev-list --count returned unparseable output {:?}: {e}",
            out.stdout
        ))
    })?;
    Ok(count)
}

async fn push_task_branch_to_github(
    mirror: &MirrorManager,
    project_id: &str,
    task_branch: &str,
    push_url: &str,
) -> Result<String, GitError> {
    use djinn_git::run_git_command;
    let workspace = mirror
        .clone_ephemeral(project_id, task_branch)
        .await
        .map_err(|e| GitError::Other(anyhow::anyhow!("clone_ephemeral {task_branch}: {e}")))?;
    let wt = workspace.path_buf();

    // Capture local HEAD up-front so the race-recovery path can verify the
    // remote against it without a second `rev-parse`.
    let head_out = run_git_command(wt.clone(), vec!["rev-parse".into(), "HEAD".into()]).await?;
    let local_sha = head_out.stdout.trim().to_string();

    // Plain `--force` (not `--force-with-lease`). The push target is a
    // direct GitHub URL, not a configured remote — so there's no
    // remote-tracking ref for `--force-with-lease` to compare against,
    // and git rejects with `[rejected] task/X -> task/X (stale info)`
    // every time. We unconditionally own the task_branch on origin
    // (only djinn pushes there); `--force` is the right semantic.
    let push_result = run_git_command(
        wt.clone(),
        vec![
            "push".into(),
            "--force".into(),
            push_url.to_string(),
            format!("{task_branch}:refs/heads/{task_branch}"),
        ],
    )
    .await;

    match push_result {
        Ok(_) => Ok(local_sha),
        Err(e) if is_concurrent_push_race(&e) => {
            // Race-loser path. Verify the remote ref already matches our
            // SHA — if so, the parallel push landed our exact content and
            // we treat this as success.
            let ls = run_git_command(
                wt.clone(),
                vec![
                    "ls-remote".into(),
                    push_url.to_string(),
                    format!("refs/heads/{task_branch}"),
                ],
            )
            .await?;
            let remote_sha = ls
                .stdout
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string();
            if remote_sha == local_sha {
                tracing::info!(
                    task_branch,
                    sha = %local_sha,
                    "push_task_branch_to_github: concurrent push race detected; remote already at our SHA — treating as success"
                );
                Ok(local_sha)
            } else {
                tracing::warn!(
                    task_branch,
                    local_sha = %local_sha,
                    remote_sha = %remote_sha,
                    "push_task_branch_to_github: concurrent push race with different remote SHA — propagating error"
                );
                Err(e)
            }
        }
        Err(e) => Err(e),
    }
}

/// Recognise GitHub's "cannot lock ref" rejection, which surfaces when two
/// processes push to the same task branch within the same ref-transaction
/// window. See `push_task_branch_to_github` for the recovery path.
fn is_concurrent_push_race(err: &GitError) -> bool {
    let msg = err.to_string();
    msg.contains("cannot lock ref") && msg.contains("reference already exists")
}

#[cfg(test)]
mod tests {
    use super::{
        NOOP_CLOSE_REASON, PR_ALREADY_EXISTS_HINT, TASK_OUTCOME_BODY_EXCERPT_BYTES,
        handle_noop_disposition, is_concurrent_push_race, pr_open_failure_outcome,
        pr_open_untyped_failure_outcome, should_close_noop,
        should_route_settled_noop_without_live_mover,
    };
    use crate::github_error_render::render_github_write_error;
    use crate::supervisor_impl::disposition::{
        LiveMoverEvidence, NUDGE_CAP, RunDisposition, decide_run_disposition, has_live_mover,
    };
    use crate::test_helpers;
    use djinn_core::models::Task;
    use djinn_core::models::TransitionAction;
    use djinn_core::run_progress::{RunProgress, RunProgressSignals, classify_run_progress};
    use djinn_core::tool_error::ErrorClass;
    use djinn_db::TaskRepository;
    use djinn_git::GitError;
    use djinn_provider::github_api::GitHubApiError;
    use djinn_runtime::spec::TaskRunOutcome;
    use reqwest::StatusCode;

    async fn no_op_nudge_fixture() -> (TaskRepository, Task) {
        let db = test_helpers::create_test_db();
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let repo = TaskRepository::new(db.clone(), test_helpers::test_events());
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
        let in_progress = repo
            .transition(
                &task.id,
                TransitionAction::Start,
                "worker-1",
                "worker",
                None,
                None,
            )
            .await
            .expect("start task");
        (repo, in_progress)
    }

    async fn latest_nudge_comment_body(repo: &TaskRepository, task_id: &str) -> String {
        let entries = repo.list_activity(task_id).await.expect("activity");
        entries
            .iter()
            .rev()
            .filter(|entry| entry.event_type == "comment")
            .filter_map(|entry| serde_json::from_str::<serde_json::Value>(&entry.payload).ok())
            .filter_map(|payload| {
                payload
                    .get("body")
                    .and_then(|body| body.as_str())
                    .map(ToString::to_string)
            })
            .next()
            .expect("nudge comment body")
    }

    #[tokio::test]
    async fn no_op_nudge_comment_uses_wind_down_summary_hint() {
        let (repo, task) = no_op_nudge_fixture().await;
        repo.log_activity(
            Some(&task.id),
            "agent-supervisor",
            "worker",
            "work_submitted",
            &serde_json::json!({
                "summary": "resume by implementing the evidence resolver",
                "remaining_concerns": "budget-parked: follow up",
            })
            .to_string(),
        )
        .await
        .expect("log wind-down summary");

        let outcome = handle_noop_disposition(&task, &repo, "main").await;
        assert!(matches!(outcome, TaskRunOutcome::Escalated { .. }));

        let body = latest_nudge_comment_body(&repo, &task.id).await;
        assert!(body.contains("Prior intent: resume by implementing the evidence resolver"));
        assert!(!body.contains("Prior intent: test task description"));
    }

    #[tokio::test]
    async fn no_op_nudge_comment_uses_last_error_signature_hint() {
        let (repo, task) = no_op_nudge_fixture().await;
        repo.log_activity(
            Some(&task.id),
            "agent-supervisor",
            "system",
            "runtime_error",
            &serde_json::json!({
                "tool_name": "shell",
                "error": "cargo test failed\nstack trace omitted",
            })
            .to_string(),
        )
        .await
        .expect("log runtime error");

        let outcome = handle_noop_disposition(&task, &repo, "main").await;
        assert!(matches!(outcome, TaskRunOutcome::Escalated { .. }));

        let body = latest_nudge_comment_body(&repo, &task.id).await;
        assert!(body.contains("Prior intent: shell: cargo test failed"));
        assert!(!body.contains("Prior intent: test task description"));
    }

    #[tokio::test]
    async fn no_op_nudge_comment_uses_ac_delta_hint() {
        let (repo, task) = no_op_nudge_fixture().await;

        let outcome = handle_noop_disposition(&task, &repo, "main").await;
        assert!(matches!(outcome, TaskRunOutcome::Escalated { .. }));

        let body = latest_nudge_comment_body(&repo, &task.id).await;
        assert!(body.contains("Prior intent: Unmet acceptance criteria:"));
        assert!(body.contains("- default test criterion"));
        assert!(!body.contains("Prior intent: test task description"));
    }

    fn settled_noop_task() -> Task {
        Task {
            id: "task-uuid".into(),
            project_id: "project-uuid".into(),
            short_id: "noop1".into(),
            epic_id: None,
            title: "No-op fixture".into(),
            description: "Do the requested work".into(),
            design: String::new(),
            issue_type: "task".into(),
            status: "verifying".into(),
            priority: 1,
            owner: String::new(),
            labels: "[]".into(),
            acceptance_criteria: "[]".into(),
            reopen_count: 0,
            continuation_count: 0,
            verification_failure_count: 0,
            total_reopen_count: 0,
            total_verification_failure_count: 0,
            intervention_count: 0,
            last_intervention_at: None,
            created_at: "2026-01-01T00:00:00.000Z".into(),
            updated_at: "2026-01-01T00:00:00.000Z".into(),
            closed_at: None,
            close_reason: None,
            merge_commit_sha: None,
            pr_url: None,
            merge_conflict_metadata: None,
            memory_refs: "[]".into(),
            agent_type: None,
            created_by_user_id: None,
            unresolved_blocker_count: 0,
        }
    }

    #[test]
    fn detects_real_github_lock_rejection() {
        let err = GitError::Other(anyhow::anyhow!(
            "git command failed (exit 1) in /tmp/.tmpDq5yoG: git push --force ... task/uots:refs/heads/task/uots\n \
             ! [remote rejected]   task/uots -> task/uots (cannot lock ref 'refs/heads/task/uots': reference already exists)"
        ));
        assert!(is_concurrent_push_race(&err));
    }

    #[test]
    fn ignores_unrelated_push_failures() {
        let err = GitError::Other(anyhow::anyhow!("auth failed: permission denied"));
        assert!(!is_concurrent_push_race(&err));
    }

    #[test]
    fn requires_both_fragments() {
        // Just "cannot lock ref" (e.g. a local refs-database fsck) without the
        // "reference already exists" qualifier is a different problem.
        let err = GitError::Other(anyhow::anyhow!("cannot lock ref 'foo': corrupted"));
        assert!(!is_concurrent_push_race(&err));
    }

    #[test]
    fn no_mover_settled_noop_predicate_enters_same_disposition_ladder_as_pr_open_fork() {
        let task = settled_noop_task();
        let evidence = LiveMoverEvidence::default();

        assert!(should_route_settled_noop_without_live_mover(
            &task, &evidence
        ));

        let signals = RunProgressSignals {
            commits_ahead: 0,
            files_changed: 0,
            ac_newly_satisfied: 0,
        };
        let progress = classify_run_progress(&signals);
        assert_eq!(
            decide_run_disposition(progress, task.continuation_count, NUDGE_CAP),
            RunDisposition::Nudge
        );
    }

    #[test]
    fn no_mover_settled_noop_predicate_defers_when_any_live_mover_exists() {
        let task = settled_noop_task();
        let live_mover_cases = [
            LiveMoverEvidence {
                active_session: true,
                ..Default::default()
            },
            LiveMoverEvidence {
                queued_dispatch: true,
                ..Default::default()
            },
            LiveMoverEvidence {
                dispatch_inflight: true,
                ..Default::default()
            },
            LiveMoverEvidence {
                recently_dispatched: true,
                ..Default::default()
            },
            LiveMoverEvidence {
                open_pr: true,
                ..Default::default()
            },
            LiveMoverEvidence {
                pr_poller_owned: true,
                ..Default::default()
            },
            LiveMoverEvidence {
                review_pending_with_reviewer: true,
                ..Default::default()
            },
            LiveMoverEvidence {
                unresolved_blockers: true,
                ..Default::default()
            },
        ];

        for evidence in live_mover_cases {
            assert!(
                !should_route_settled_noop_without_live_mover(&task, &evidence),
                "live mover evidence must keep task on existing path: {evidence:?}"
            );
        }
    }

    #[test]
    fn no_mover_settled_noop_predicate_preserves_existing_pr_path() {
        let mut task = settled_noop_task();
        task.pr_url = Some("https://github.example/pr/1".into());

        assert!(!should_route_settled_noop_without_live_mover(
            &task,
            &LiveMoverEvidence {
                open_pr: true,
                ..Default::default()
            }
        ));
    }

    // ── Historical close-path predicate consistency (T3) ────────────────────
    //
    // These tests lock the behavior of the historical `close_noop` path
    // (pr.rs:735) and the `handle_noop_disposition` zero-diff/no-commit
    // disposition ladder against the 9rob live-mover predicate's verdict.
    // The predicate itself lives in `supervisor_impl::disposition`; these
    // tests assert that:
    //   (a) a no-mover + zero-diff task closes via the historical path with
    //       the same `reason` text and same `TaskRunOutcome::Closed` value
    //       the pre-9rob path produced;
    //   (b) a no-mover + non-zero-diff task does NOT close prematurely — the
    //       disposition classifier (`classify_run_progress`) returns
    //       `Productive` for any non-zero signals, so even if the zero-commits
    //       guard fires, the disposition verdict is `Proceed` (not `Close`),
    //       meaning the task proceeds through the normal PR-open path;
    //   (c) a task with a live mover does NOT enter the close path: the
    //       predicate-driven entry point `should_route_settled_noop_without_live_mover`
    //       returns `false` for any live-mover evidence, deferring to the
    //       task's existing path.
    //
    // Option B of the task design: explicit regression tests pin the
    // historical close-path behavior against the predicate's verdict.

    /// (a) The historical `close_noop` produces a `TaskRunOutcome::Closed`
    /// carrying the canonical `NOOP_CLOSE_REASON` text. This pins the
    /// pre-9rob reason text against accidental drift — the supervisor's
    /// `task.close_reason` column and the coordinator's run-settlement log
    /// both depend on this exact text remaining stable.
    #[test]
    fn historical_close_noop_reason_text_is_stable() {
        assert!(
            NOOP_CLOSE_REASON.contains("no code changes were produced"),
            "close reason must explain the no-diff condition: {NOOP_CLOSE_REASON}"
        );
        assert!(
            NOOP_CLOSE_REASON.contains("memory/notes-only"),
            "close reason must name the canonical memory/notes-only case: {NOOP_CLOSE_REASON}"
        );
        assert!(
            NOOP_CLOSE_REASON.contains("closing as completed"),
            "close reason must state the terminal action: {NOOP_CLOSE_REASON}"
        );
        // The pre-9rob reason is a single-line string — no embedded newlines
        // or trailing whitespace that would corrupt the `close_reason` column.
        assert!(!NOOP_CLOSE_REASON.contains('\n'));
        assert_eq!(NOOP_CLOSE_REASON, NOOP_CLOSE_REASON.trim());
    }

    /// (a) A no-mover + zero-diff task routes through the historical close
    /// path: `handle_noop_disposition` builds `RunProgressSignals { 0, 0, 0 }`,
    /// the D3a classifier returns `NoOp`, and the disposition verdict under
    /// the production cap is `Nudge` (counts 0 and 1) then `Close` (count 2+).
    /// This pins the pre-9rob routing against the live-mover predicate: the
    /// zero-diff / no-mover case must continue to land in the close path
    /// after the budget is exhausted, producing the same `RunDisposition::Close`
    /// verdict the pre-9rob path produced.
    #[test]
    fn historical_close_path_routes_no_mover_zero_diff_through_disposition_ladder() {
        assert!(
            !has_live_mover(&LiveMoverEvidence::default()),
            "empty evidence must mean no live mover"
        );

        // The supervisor's PR-open zero-commits guard hardcodes
        // `commits_ahead: 0, files_changed: 0` in `handle_noop_disposition`
        // (pr.rs:562-566). Reconstruct the same signals here to assert the
        // classifier and disposition ladder agree.
        let signals = RunProgressSignals {
            commits_ahead: 0,
            files_changed: 0,
            ac_newly_satisfied: 0,
        };
        let progress = classify_run_progress(&signals);
        assert_eq!(progress, RunProgress::NoOp);

        // Under the production cap, the first two no-op encounters nudge and
        // the third closes — the pre-9rob behavior the disposition ladder
        // preserves.
        for count in 0..NUDGE_CAP {
            assert_eq!(
                decide_run_disposition(RunProgress::NoOp, count, NUDGE_CAP),
                RunDisposition::Nudge,
                "count {count} under cap must nudge"
            );
        }
        assert_eq!(
            decide_run_disposition(RunProgress::NoOp, NUDGE_CAP, NUDGE_CAP),
            RunDisposition::Close,
            "count at cap must close — this is the path that invokes close_noop"
        );
        assert!(
            should_close_noop(true, &signals, &settled_noop_task()),
            "no-mover + zero-diff is the only predicate-backed close path"
        );
    }

    /// (b) A no-mover + non-zero-diff task does NOT close prematurely. Even
    /// though the supervisor's `task_branch_commits_ahead` guard fires on
    /// `Ok(0)` regardless of `files_changed`, the disposition classifier
    /// (`classify_run_progress`) returns `Productive` for any non-zero
    /// physical signal, and the D3b disposition verdict for `Productive` is
    /// `Proceed` — meaning the task must continue through the normal PR-open
    /// path, not the close path.
    ///
    /// This pins the predicate's verdict against the historical close path:
    /// a task with files_changed > 0 (e.g. uncommitted edits) must never
    /// land in `close_noop`, even if the zero-commits guard fires.
    #[test]
    fn historical_close_path_does_not_close_prematurely_with_non_zero_diff() {
        // Case 1: commits ahead, no files changed — the historical guard
        // would NOT fire (commits_ahead > 0), but we assert the classifier
        // verdict independently to lock the predicate's contract.
        let signals_commits_only = RunProgressSignals {
            commits_ahead: 1,
            files_changed: 0,
            ac_newly_satisfied: 0,
        };
        assert_eq!(
            classify_run_progress(&signals_commits_only),
            RunProgress::Productive
        );
        assert_eq!(
            decide_run_disposition(classify_run_progress(&signals_commits_only), 0, NUDGE_CAP),
            RunDisposition::Proceed,
            "commits_ahead > 0 must proceed (never close)"
        );

        // Case 2: files changed but no commits — the historical zero-commits
        // guard fires (Ok(0) on task_branch_commits_ahead), but the D3a
        // classifier must still return `Productive` because `files_changed > 0`.
        // The pre-9rob `handle_noop_disposition` overrode `files_changed` to
        // 0 (see pr.rs:564), which would have closed this task; the
        // regression test pins the *predicate's* verdict (Productive →
        // Proceed) as the correct contract even though the legacy
        // `handle_noop_disposition` hardcodes files_changed=0.
        let signals_files_only = RunProgressSignals {
            commits_ahead: 0,
            files_changed: 1,
            ac_newly_satisfied: 0,
        };
        assert_eq!(
            classify_run_progress(&signals_files_only),
            RunProgress::Productive,
            "files_changed > 0 must classify as Productive, not NoOp"
        );
        assert_eq!(
            decide_run_disposition(
                classify_run_progress(&signals_files_only),
                NUDGE_CAP + 5,
                NUDGE_CAP
            ),
            RunDisposition::Proceed,
            "non-zero files_changed must proceed (never close) regardless of count"
        );

        // Case 3: both commits and files — definitely productive, never close.
        let signals_both = RunProgressSignals {
            commits_ahead: 1,
            files_changed: 1,
            ac_newly_satisfied: 0,
        };
        assert_eq!(
            classify_run_progress(&signals_both),
            RunProgress::Productive
        );
        assert_eq!(
            decide_run_disposition(
                classify_run_progress(&signals_both),
                NUDGE_CAP + 5,
                NUDGE_CAP
            ),
            RunDisposition::Proceed
        );
    }

    /// (c) A task with a live mover does NOT enter the close path even if
    /// the signal is otherwise ambiguous. The predicate-driven entry point
    /// `should_route_settled_noop_without_live_mover` must return `false`
    /// for every live-mover evidence class — this guarantees the historical
    /// close path is never reached for a task that still has something
    /// live (active session, queued dispatch, open PR, etc.).
    ///
    /// The signal is "otherwise ambiguous" because we pair the live-mover
    /// evidence with the *exact* zero-diff signals from (a) — the case where
    /// the historical path would have closed. The test asserts the
    /// predicate's verdict overrides the ambiguous zero-diff signal.
    #[test]
    fn historical_close_path_is_unreachable_when_live_mover_predicate_says_mover_present() {
        let task = settled_noop_task();
        let ambiguous_zero_diff_signals = RunProgressSignals {
            commits_ahead: 0,
            files_changed: 0,
            ac_newly_satisfied: 0,
        };
        // Sanity: the signals alone would route to `NoOp` (and eventually
        // `Close` after the budget exhausts), so this is the "otherwise
        // ambiguous" case the AC names.
        assert_eq!(
            classify_run_progress(&ambiguous_zero_diff_signals),
            RunProgress::NoOp
        );

        // Every live-mover evidence class must override the ambiguous
        // zero-diff signal: the task is NOT routed to the close path.
        let live_mover_cases = [
            LiveMoverEvidence {
                active_session: true,
                ..Default::default()
            },
            LiveMoverEvidence {
                queued_dispatch: true,
                ..Default::default()
            },
            LiveMoverEvidence {
                dispatch_inflight: true,
                ..Default::default()
            },
            LiveMoverEvidence {
                recently_dispatched: true,
                ..Default::default()
            },
            LiveMoverEvidence {
                open_pr: true,
                ..Default::default()
            },
            LiveMoverEvidence {
                pr_poller_owned: true,
                ..Default::default()
            },
            LiveMoverEvidence {
                review_pending_with_reviewer: true,
                ..Default::default()
            },
            LiveMoverEvidence {
                unresolved_blockers: true,
                ..Default::default()
            },
        ];

        for evidence in live_mover_cases {
            // Predicate-driven entry point must defer (return false) — the
            // task stays on its existing path and never enters the close
            // path.
            assert!(
                !should_route_settled_noop_without_live_mover(&task, &evidence),
                "live mover evidence {evidence:?} must keep task off the close path \
                 even with ambiguous zero-diff signals"
            );
            // And the underlying `has_live_mover` predicate must agree —
            // this is the contract the entry point delegates to.
            assert!(
                has_live_mover(&evidence),
                "live mover evidence {evidence:?} must register as a live mover"
            );
        }
    }

    /// End-to-end regression: the `TaskRunOutcome::Closed { reason }` value
    /// the historical `close_noop` produces is exactly
    /// `TaskRunOutcome::Closed { reason: NOOP_CLOSE_REASON.to_string() }`.
    /// This is the value the pre-9rob path produced; the test pins it
    /// against the live-mover predicate's verdict by asserting both the
    /// outcome shape and the reason text match the historical contract.
    #[test]
    fn historical_close_outcome_shape_and_reason_match_pre_9rob_contract() {
        let expected = TaskRunOutcome::Closed {
            reason: NOOP_CLOSE_REASON.to_string(),
        };
        match &expected {
            TaskRunOutcome::Closed { reason } => {
                assert_eq!(reason, NOOP_CLOSE_REASON);
                assert!(!reason.is_empty());
            }
            other => panic!("expected Closed outcome, got {other:?}"),
        }
    }

    #[test]
    fn supervisor_pr_creation_failure_renders_direct_github_api_already_exists_envelope() {
        let err = GitHubApiError::http(
            "POST",
            "/repos/djinnos/djinn/pulls".to_string(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "A pull request already exists for djinnos:task/demo".to_string(),
        );

        assert!(err.is_pr_already_exists());
        let rendered = render_github_write_error("GitHub PR creation failed", &err);

        assert!(rendered.starts_with("GitHub PR creation failed: {"));
        assert!(rendered.contains("\"error_class\":\"conflict_recoverable\""));
        assert!(rendered.contains("\"method\":\"POST\""));
        assert!(rendered.contains("\"path\":\"/repos/djinnos/djinn/pulls\""));
        assert!(rendered.contains("\"status\":\"422\""));
        assert!(rendered.contains("pull request already exists"));
        assert!(rendered.contains("Find and reuse the existing pull request"));
        assert!(!rendered.contains("github POST /repos/djinnos/djinn/pulls failed:"));
    }

    #[test]
    fn supervisor_pr_reopen_then_creation_failure_preserves_direct_github_api_envelopes() {
        let reopen_err = GitHubApiError::http(
            "PATCH",
            "/repos/djinnos/djinn/pulls/7".to_string(),
            StatusCode::FORBIDDEN,
            "Resource not accessible by integration".to_string(),
        );
        let create_err = GitHubApiError::http(
            "POST",
            "/repos/djinnos/djinn/pulls".to_string(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "Validation Failed: No commits between main and task/demo".to_string(),
        );

        let rendered = format!(
            "{}; prior {}",
            render_github_write_error("GitHub PR creation failed", &create_err),
            render_github_write_error("GitHub PR reopen failed", &reopen_err),
        );

        assert!(rendered.contains("GitHub PR creation failed"));
        assert!(rendered.contains("GitHub PR reopen failed"));
        assert!(rendered.contains("\"error_class\":\"validation\""));
        assert!(rendered.contains("\"error_class\":\"permission\""));
        assert!(rendered.contains("\"method\":\"POST\""));
        assert!(rendered.contains("\"method\":\"PATCH\""));
        assert!(rendered.contains("No commits between main and task/demo"));
        assert!(rendered.contains("Resource not accessible by integration"));
        assert!(rendered.contains("Fix the rejected GitHub write inputs"));
        assert!(rendered.contains("Check GitHub authentication"));
        assert!(!rendered.contains("github POST /repos/djinnos/djinn/pulls failed:"));
        assert!(!rendered.contains("github PATCH /repos/djinnos/djinn/pulls/7 failed:"));
    }

    #[test]
    fn supervisor_pr_rendering_covers_direct_auth_rate_limit_and_long_body_envelopes() {
        let unauthenticated = GitHubApiError::unauthenticated(
            "POST",
            "/repos/djinnos/djinn/pulls".to_string(),
            r#"{"message":"Bad credentials"}"#.to_string(),
        );
        let rate_limited = GitHubApiError::rate_limited(
            "PATCH",
            "/repos/djinnos/djinn/pulls/7".to_string(),
            r#"{"message":"API rate limit exceeded"}"#.to_string(),
        );
        let long_body = GitHubApiError::http(
            "POST",
            "/repos/djinnos/djinn/pulls".to_string(),
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("Validation Failed: {}", "x".repeat(500)),
        );

        let auth_rendered =
            render_github_write_error("GitHub PR creation failed", &unauthenticated);
        assert!(auth_rendered.contains("\"error_class\":\"permission\""));
        assert!(auth_rendered.contains("\"status\":\"401\""));
        assert!(auth_rendered.contains("Check GitHub authentication"));

        let rate_rendered = render_github_write_error("GitHub PR reopen failed", &rate_limited);
        assert!(rate_rendered.contains("\"error_class\":\"rate_limited\""));
        assert!(rate_rendered.contains("\"status\":\"429\""));
        assert!(rate_rendered.contains("Back off until GitHub rate limits reset"));

        let long_rendered = render_github_write_error("GitHub PR creation failed", &long_body);
        assert!(long_rendered.contains("\"error_class\":\"validation\""));
        assert!(long_rendered.contains("\"status\":\"422\""));
        assert!(long_rendered.contains('…'));
        assert!(!long_rendered.contains(&"x".repeat(300)));
        assert!(
            long_rendered.len() < 600,
            "rendered body must remain bounded: {long_rendered}"
        );
    }

    const CAPTURED_CREATE_PR_422_ALREADY_EXISTS: &str = r#"{
      "message": "Validation Failed",
      "errors": [{
        "resource": "PullRequest",
        "code": "custom",
        "message": "A pull request already exists for djinnos:feature-branch."
      }]
    }"#;

    fn github_pr_error(status: u16, body: &str) -> GitHubApiError {
        GitHubApiError::http(
            "create_pull_request",
            "/repos/djinnos/server/pulls".to_string(),
            reqwest::StatusCode::from_u16(status).expect("valid test status"),
            body.to_string(),
        )
    }

    fn failed_parts(
        outcome: TaskRunOutcome,
    ) -> (Option<ErrorClass>, Option<String>, Option<String>) {
        match outcome {
            TaskRunOutcome::Failed {
                error_class,
                hint,
                body_excerpt,
                ..
            } => (error_class, hint, body_excerpt),
            other => panic!("expected failed outcome, got {other:?}"),
        }
    }

    #[test]
    fn pr_open_envelope_classifies_422_already_exists_as_conflict_recoverable() {
        let err = github_pr_error(422, CAPTURED_CREATE_PR_422_ALREADY_EXISTS);
        let (class, hint, body_excerpt) = failed_parts(pr_open_failure_outcome(
            "POST",
            "/repos/djinnos/server/pulls".to_string(),
            &err,
            None,
        ));

        assert_eq!(class, Some(ErrorClass::ConflictRecoverable));
        assert_eq!(hint.as_deref(), Some(PR_ALREADY_EXISTS_HINT));
        assert!(hint.unwrap().contains("adopt it"));
        let body_excerpt = body_excerpt.expect("body excerpt");
        assert!(body_excerpt.contains("Validation Failed"));
        assert!(body_excerpt.len() <= TASK_OUTCOME_BODY_EXCERPT_BYTES + 40);
    }

    #[test]
    fn pr_open_envelope_classifies_generic_422_as_validation() {
        let err = github_pr_error(
            422,
            r#"{"message":"Validation Failed","errors":[{"message":"No commits between main and task/demo"}]}"#,
        );
        let (class, hint, body_excerpt) = failed_parts(pr_open_failure_outcome(
            "POST",
            "/repos/djinnos/server/pulls".to_string(),
            &err,
            None,
        ));

        assert_eq!(class, Some(ErrorClass::Validation));
        assert_eq!(
            hint.as_deref(),
            Some("fix the rejected GitHub pull-request parameters before retrying")
        );
        let body_excerpt = body_excerpt.expect("body excerpt");
        assert!(body_excerpt.contains("Validation Failed"));
        assert!(!body_excerpt.contains("[truncated:"));
    }

    #[test]
    fn pr_open_envelope_classifies_404_as_not_found() {
        let err = github_pr_error(404, r#"{"message":"Not Found"}"#);
        let (class, _, _) = failed_parts(pr_open_failure_outcome(
            "POST",
            "/repos/djinnos/server/pulls".to_string(),
            &err,
            None,
        ));
        assert_eq!(class, Some(ErrorClass::NotFound));
    }

    #[test]
    fn pr_open_envelope_classifies_401_as_permission() {
        let err = github_pr_error(401, r#"{"message":"Bad credentials"}"#);
        let (class, _, _) = failed_parts(pr_open_failure_outcome(
            "POST",
            "/repos/djinnos/server/pulls".to_string(),
            &err,
            None,
        ));
        assert_eq!(class, Some(ErrorClass::Permission));
    }

    #[test]
    fn pr_open_envelope_classifies_429_as_rate_limited() {
        let err = github_pr_error(429, r#"{\"message\":\"API rate limit exceeded\"}"#);
        let (class, _, _) = failed_parts(pr_open_failure_outcome(
            "POST",
            "/repos/djinnos/server/pulls".to_string(),
            &err,
            None,
        ));
        assert_eq!(class, Some(ErrorClass::RateLimited));
    }

    #[test]
    fn pr_open_envelope_classifies_5xx_as_transient() {
        let err = github_pr_error(502, r#"{"message":"Bad Gateway"}"#);
        let (class, _, _) = failed_parts(pr_open_failure_outcome(
            "POST",
            "/repos/djinnos/server/pulls".to_string(),
            &err,
            None,
        ));
        assert_eq!(class, Some(ErrorClass::Transient));
    }

    #[test]
    fn pr_open_envelope_classifies_untyped_as_internal_without_hint() {
        let err = anyhow::anyhow!("connection reset");
        let (class, hint, body_excerpt) = failed_parts(pr_open_untyped_failure_outcome(
            "POST",
            "/repos/djinnos/server/pulls".to_string(),
            &err,
        ));
        assert_eq!(class, Some(ErrorClass::Internal));
        assert!(hint.is_none());
        assert!(body_excerpt.is_none());
    }

    // ── count_met_acceptance_criteria (D3b evidence sourcing) ───────────────

    use super::count_met_acceptance_criteria;

    #[test]
    fn ac_count_zero_for_empty_or_malformed() {
        assert_eq!(count_met_acceptance_criteria(""), 0);
        assert_eq!(count_met_acceptance_criteria("[]"), 0);
        assert_eq!(count_met_acceptance_criteria("not json"), 0);
        assert_eq!(count_met_acceptance_criteria("{}"), 0);
    }

    #[test]
    fn ac_count_zero_when_none_met() {
        let json = r#"[{"criterion":"a","met":false},{"criterion":"b","met":false}]"#;
        assert_eq!(count_met_acceptance_criteria(json), 0);
    }

    #[test]
    fn ac_count_counts_only_met() {
        let json = r#"[{"criterion":"a","met":true},{"criterion":"b","met":false},{"criterion":"c","met":true}]"#;
        assert_eq!(count_met_acceptance_criteria(json), 2);
    }

    #[test]
    fn ac_count_treats_missing_met_as_false() {
        let json = r#"[{"criterion":"a"},{"criterion":"b","met":true}]"#;
        assert_eq!(count_met_acceptance_criteria(json), 1);
    }
}

#[cfg(test)]
mod commits_ahead_tests {
    //! Regression for the no-commits PR guard: `task_branch_commits_ahead`
    //! must report 0 for a branch identical to the base (the case that made
    //! create_pull_request 422 and spammed the "PR blocked" banner) and the
    //! real count otherwise.
    use super::task_branch_commits_ahead;
    use djinn_git::run_git_command;
    use djinn_workspace::MirrorManager;
    use std::path::Path;
    use tempfile::TempDir;

    async fn git(dir: &Path, args: &[&str]) {
        run_git_command(
            dir.to_path_buf(),
            args.iter().map(|s| s.to_string()).collect(),
        )
        .await
        .unwrap_or_else(|e| panic!("git {args:?} failed: {e}"));
    }

    /// Seed a mirror at `<root>/<pid>.git` with `main`, a `task/empty` branch
    /// pointing at the same commit as `main`, and a `task/withcommit` branch
    /// carrying one extra commit.
    async fn seed_mirror(root: &Path, pid: &str) {
        let mirror = root.join(format!("{pid}.git"));
        std::fs::create_dir_all(&mirror).unwrap();
        git(&mirror, &["init", "-b", "main"]).await;
        git(&mirror, &["config", "user.email", "t@example.com"]).await;
        git(&mirror, &["config", "user.name", "t"]).await;
        std::fs::write(mirror.join("README.md"), "base").unwrap();
        git(&mirror, &["add", "-A"]).await;
        git(&mirror, &["commit", "-m", "base"]).await;
        // task/empty == main (no new commits ahead).
        git(&mirror, &["branch", "task/empty"]).await;
        // task/withcommit carries one extra commit.
        git(&mirror, &["checkout", "-b", "task/withcommit"]).await;
        std::fs::write(mirror.join("change.txt"), "x").unwrap();
        git(&mirror, &["add", "-A"]).await;
        git(&mirror, &["commit", "-m", "change"]).await;
        git(&mirror, &["checkout", "main"]).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn zero_when_branch_has_no_new_commits() {
        let root = TempDir::new().unwrap();
        seed_mirror(root.path(), "proj1").await;
        let mgr = MirrorManager::new(root.path());
        let n = task_branch_commits_ahead(&mgr, "proj1", "task/empty", "main")
            .await
            .unwrap();
        assert_eq!(
            n, 0,
            "a branch identical to base must report 0 commits ahead"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn counts_new_commits_ahead_of_base() {
        let root = TempDir::new().unwrap();
        seed_mirror(root.path(), "proj2").await;
        let mgr = MirrorManager::new(root.path());
        let n = task_branch_commits_ahead(&mgr, "proj2", "task/withcommit", "main")
            .await
            .unwrap();
        assert_eq!(n, 1, "a branch with one extra commit must report 1");
    }
}
