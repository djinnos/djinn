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
    // body's in_progress caller and the coordinator's approved
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
                            if let Some(outcome) = adopt_recorded_pr_after_create_conflict(
                                &e,
                                &task_repo,
                                task,
                                &merge_result_commit_sha,
                            )
                            .await
                            {
                                return outcome;
                            }
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
                if let Some(outcome) = adopt_recorded_pr_after_create_conflict(
                    &e,
                    &task_repo,
                    task,
                    &merge_result_commit_sha,
                )
                .await
                {
                    return outcome;
                }
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

/// Last-resort self-heal when `create_pull_request` still reports
/// "already exists" after its internal adoption retries: a concurrent
/// PR-open (the in-pod supervisor and the coordinator's approved re-cycle
/// can race) may have created the PR and recorded it on the task row.
/// Adopt that URL and finish the flow instead of failing the run — the
/// board is already correct, and a `Failed { pr_open }` here surfaces a
/// conflict envelope to the operator for a PR that opened fine (task mbfw,
/// 2026-07-16, during GitHub's degraded-REST-API incident).
async fn adopt_recorded_pr_after_create_conflict(
    err: &GitHubApiError,
    task_repo: &TaskRepository,
    task: &djinn_core::models::Task,
    head_sha: &str,
) -> Option<TaskRunOutcome> {
    if !err.is_pr_already_exists() {
        return None;
    }
    let pr_url = match task_repo.get(&task.id).await {
        Ok(Some(fresh)) => fresh.pr_url.filter(|url| !url.is_empty()),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                "supervisor PR-open: failed to re-read task while healing create conflict"
            );
            None
        }
    }?;

    // Mirror the success path: the racing winner set pr_url before
    // transitioning, so re-issue the transition here in case it crashed in
    // between — otherwise the task sits in `approved` and re-runs PR-open
    // every tick. Already-transitioned tasks warn-skip exactly like the
    // success path.
    if let Err(e) = task_repo
        .transition(
            &task.id,
            TransitionAction::PrCreated,
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
            "supervisor PR-open: pr_created transition skipped while healing create conflict"
        );
    }

    tracing::info!(
        task_id = %task.short_id,
        pr_url = %pr_url,
        "Supervisor: PR create conflicted but a concurrent open already recorded the PR — adopted"
    );
    Some(TaskRunOutcome::PrOpened {
        url: pr_url,
        sha: head_sha.to_string(),
    })
}

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
    // state. Only `in_progress` (the supervisor-body caller) can
    // be released without stepping on a terminal-action path; the coordinator
    // re-cycle reaches `open_pr` from `approved`, which we must NOT bounce back
    // to open. If no safe release action exists, downgrade Nudge → Close.
    let release_action = match task.status.as_str() {
        "in_progress" => Some(TransitionAction::Release),
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
#[allow(dead_code)]
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

#[allow(dead_code)]
pub(super) fn should_route_settled_noop_without_live_mover(
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
pub(super) fn should_close_noop(
    predicate_says_no_mover: bool,
    signals: &djinn_core::run_progress::RunProgressSignals,
    _task: &djinn_core::models::Task,
) -> bool {
    predicate_says_no_mover && signals.commits_ahead == 0 && signals.files_changed == 0
}

pub(super) async fn close_noop(
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
    // body's in_progress caller and the coordinator's approved
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

pub(crate) async fn push_task_branch_to_github(
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
pub(crate) fn is_concurrent_push_race(err: &GitError) -> bool {
    let msg = err.to_string();
    msg.contains("cannot lock ref") && msg.contains("reference already exists")
}

#[cfg(test)]
#[path = "pr_tests.rs"]
mod tests;
