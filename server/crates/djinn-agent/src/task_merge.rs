use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use crate::context::AgentContext;
use crate::knowledge_promotion::{
    KnowledgeCleanupReason, KnowledgePromotionDecision, apply_task_knowledge_decision,
};
use djinn_core::models::{SessionStatus, TransitionAction};
use djinn_db::{ProjectRepository, SessionRepository, TaskRepository};
use djinn_git::{GitError, MergeResult};
use djinn_provider::github_api::{CreatePrParams, GitHubApiClient, PrState};
use djinn_provider::github_app::{
    app_id as github_app_id, get_installation_token, installations::invalidate_cache,
};
use djinn_workspace::MirrorManager;

/// Build the HTTPS push URL for a GitHub repo authenticated by a GitHub App
/// **installation** access token.
///
/// The resulting URL uses the `x-access-token` basic-auth username that
/// GitHub documents for installation tokens, and encodes `owner`/`repo`
/// unchanged (we assume callers have already normalised them). Commits
/// pushed through this URL are attributed to the App's bot identity
/// (`djinn-bot[bot]`).
pub(crate) fn build_app_push_url(owner: &str, repo: &str, installation_token: &str) -> String {
    let repo = repo.trim_end_matches(".git");
    format!("https://x-access-token:{installation_token}@github.com/{owner}/{repo}.git")
}

/// Bot identity used when committing/pushing through the GitHub App. The
/// canonical no-reply email form is `<app-id>+djinn-bot[bot]@users.noreply.github.com`.
fn bot_identity() -> (String, String) {
    let app_id = djinn_provider::github_app::app_id()
        .map(|id| id.to_string())
        .unwrap_or_else(|_| "0".to_string());
    (
        "djinn-bot[bot]".to_string(),
        format!("{app_id}+djinn-bot[bot]@users.noreply.github.com"),
    )
}

const MERGE_CONFLICT_PREFIX: &str = "merge_conflict:";
const MERGE_VALIDATION_PREFIX: &str = "merge_validation_failed:";
const EMPTY_BRANCH_PREFIX: &str = "empty_branch:";

/// Callback type for running a pre-merge verification gate.
///
/// Takes `(task_id, project_path)` and returns `Ok(())` if verification
/// passes, or `Err(feedback)` with a human-readable failure description.
/// Provided by the server layer so that merge orchestration has no dependency
/// on `crate::actors`.
pub(crate) type VerificationGateFn = Box<
    dyn Fn(String, String) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>>
        + Send
        + Sync,
>;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct MergeConflictMetadata {
    conflicting_files: Vec<String>,
    base_branch: String,
    merge_target: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct MergeValidationFailureMetadata {
    base_branch: String,
    merge_target: String,
    command: String,
    cwd: String,
    exit_code: i32,
    stdout: String,
    stderr: String,
}

/// Transition actions to use for each merge outcome.
/// Allows coordinator-driven approval flows to reuse the same merge logic.
pub(crate) struct MergeActions {
    pub(crate) approve: TransitionAction,
    pub(crate) conflict: TransitionAction,
    pub(crate) release: TransitionAction,
    /// Action when pre-merge verification fails. Falls back to `release` if None.
    pub(crate) verification_fail: Option<TransitionAction>,
    /// Action when GitHub PR creation fails (infra/auth error, not a code issue).
    /// Falls back to `release` if None.
    pub(crate) pr_creation_fail: Option<TransitionAction>,
    /// Action when a GitHub PR is created successfully. The task should wait for
    /// the PR to be merged rather than closing immediately. Falls back to
    /// `approve` if None (used by the direct-push path where there is no PR).
    pub(crate) pr_created: Option<TransitionAction>,
}

/// Attempt to create a GitHub PR for the task branch.
///
/// Returns `Some(pr_url)` when the PR was created successfully, or `None` when
/// the GitHub App credential is absent (caller should fall back to direct-push).
/// On failure to create the PR (credential present but API error) returns an
/// `Err` so the caller can surface it.
async fn try_create_github_pr(
    task_id: &str,
    base_branch: &str,
    merge_target: &str,
    project_dir: &Path,
    project_id: &str,
    app_state: &AgentContext,
) -> Result<Option<String>, String> {
    // The GitHub App must be configured for any PR-creation path to work.
    // Fall back to direct-push if the server is running without an App
    // (e.g. local dev): callers treat `Ok(None)` as "no App available".
    if github_app_id().is_err() {
        return Ok(None);
    }

    // Resolve owner/repo + installation id from the persisted project row
    // (set at `project_add_from_github` time, Migrations 2 + 4). Legacy
    // rows without either column are not pushable through the App; fail
    // cleanly rather than falling back to a user-token push.
    let project_repo = ProjectRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let (owner, repo_name) = match project_repo.get_github_coords(project_id).await {
        Ok(Some(coords)) => coords,
        Ok(None) => {
            return Err(format!(
                "project {project_id} has no github_owner/github_repo persisted — \
                 re-add this project via `project_add_from_github` so the Djinn \
                 GitHub App can push on its behalf"
            ));
        }
        Err(e) => {
            return Err(format!(
                "failed to read github coords for project {project_id}: {e}"
            ));
        }
    };

    let installation_id = match project_repo.get_installation_id(project_id).await {
        Ok(Some(id)) => id,
        Ok(None) => {
            return Err(format!(
                "project {project_id} ({owner}/{repo_name}) has no cached installation_id — \
                 re-add this project via `project_add_from_github` so the Djinn GitHub \
                 App installation is persisted on the row"
            ));
        }
        Err(e) => {
            return Err(format!(
                "failed to read installation_id for project {project_id}: {e}"
            ));
        }
    };

    // Mint a 1-hour installation access token for the push URL (cached in
    // process — see `github_app::installations`).
    let install_token = get_installation_token(installation_id)
        .await
        .map_err(|e| format!("could not mint installation token: {e}"))?;
    let push_url = build_app_push_url(&owner, &repo_name, &install_token.token);

    // Prune stale remote-tracking refs so --force-with-lease doesn't
    // reject pushes to branch names that were deleted after a prior merge.
    let _ = djinn_git::run_git_command(
        project_dir.to_path_buf(),
        vec!["fetch".into(), "--prune".into(), "origin".into()],
    )
    .await;

    // Verify the local branch exists before pushing — the branch can be lost
    // if prepare_worktree runs between the reviewer's teardown and this push
    // (it deletes "stale" branches whose worktree is gone).
    let branch_check = djinn_git::run_git_command(
        project_dir.to_path_buf(),
        vec![
            "show-ref".into(),
            "--verify".into(),
            "--quiet".into(),
            format!("refs/heads/{base_branch}"),
        ],
    )
    .await;
    if branch_check.is_err() {
        return Err(format!(
            "local branch {base_branch} does not exist — it may have been deleted \
             by a concurrent prepare_worktree or intervention between approval and \
             PR push. The task should be reopened so a worker can redo the work."
        ));
    }

    // Push the branch through the App-authenticated URL. Commits authored
    // by the agents inherit the repo's `user.name`/`user.email` (set to
    // `djinn-bot[bot]` at clone time), but we also inject them here via
    // `-c` flags so a worktree created before that config landed still
    // pushes under the bot identity.
    let (bot_name, bot_email) = bot_identity();
    let push_args = |url: &str| -> Vec<String> {
        vec![
            "-c".into(),
            format!("user.name={bot_name}"),
            "-c".into(),
            format!("user.email={bot_email}"),
            "push".into(),
            "--force-with-lease".into(),
            url.to_string(),
            format!("{base_branch}:{base_branch}"),
        ]
    };

    let first_attempt =
        djinn_git::run_git_command(project_dir.to_path_buf(), push_args(&push_url)).await;
    if let Err(first_err) = first_attempt {
        // Installation tokens are 1h TTL with a 5-min refresh margin, but a
        // push that straddles the cache-expiry boundary can still land on a
        // revoked token. Invalidate the cache, refetch, and retry once.
        tracing::warn!(
            task_id = %task_id,
            owner = %owner,
            repo = %repo_name,
            installation_id,
            error = %first_err,
            "github-app push failed — refreshing installation token and retrying once"
        );
        invalidate_cache(installation_id);
        let refreshed = get_installation_token(installation_id)
            .await
            .map_err(|e| format!("failed to refresh installation token after push error: {e}"))?;
        let retry_url = build_app_push_url(&owner, &repo_name, &refreshed.token);
        djinn_git::run_git_command(project_dir.to_path_buf(), push_args(&retry_url))
            .await
            .map_err(|e| format!("failed to push branch {base_branch} to origin: {e}"))?;
    }

    // Load the task for PR body construction.
    let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let task = task_repo
        .get(task_id)
        .await
        .ok()
        .flatten()
        .ok_or_else(|| "task not found for PR body".to_string())?;

    // Check whether the branch has any commits not already on the merge target.
    // If not, the work was absorbed via prerequisite merges and there is nothing
    // to PR — return a distinct error so the caller can close the task.
    let rev_list = djinn_git::run_git_command(
        project_dir.to_path_buf(),
        vec![
            "rev-list".into(),
            "--count".into(),
            format!("origin/{merge_target}..{base_branch}"),
        ],
    )
    .await
    .map(|o| o.stdout.trim().to_string())
    .unwrap_or_else(|_| String::new());

    if rev_list == "0" {
        return Err(format!(
            "{EMPTY_BRANCH_PREFIX} no commits between {merge_target} and {base_branch}"
        ));
    }

    // Build acceptance criteria checklist from the JSON array.
    let criteria_lines: String = {
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum AcItem {
            Text(String),
            Structured {
                criterion: String,
                #[serde(default)]
                met: bool,
            },
        }

        match serde_json::from_str::<Vec<AcItem>>(&task.acceptance_criteria) {
            Ok(items) if !items.is_empty() => items
                .iter()
                .map(|item| match item {
                    AcItem::Text(t) => format!("- [ ] {t}\n"),
                    AcItem::Structured { criterion, met } => {
                        let check = if *met { "x" } else { " " };
                        format!("- [{check}] {criterion}\n")
                    }
                })
                .collect(),
            _ => String::new(),
        }
    };

    let pr_body = format!(
        "## Summary\n{description}\n\n## Acceptance Criteria\n{criteria}\n---\nDjinn task: {short_id}",
        description = task.description,
        criteria = criteria_lines,
        short_id = task.short_id,
    );

    let commit_type = if task.issue_type == "task" {
        "chore"
    } else {
        "feat"
    };
    let pr_title = format!("{}({}): {}", commit_type, task.short_id, task.title);

    // PR creation goes through the same installation token used for the
    // push, so the PR and its commits are attributed to the App bot instead
    // of an authenticated user.
    let github_client = GitHubApiClient::for_installation(installation_id);
    let head_ref = format!("{owner}:{base_branch}");

    // Before creating a new PR, check if one already exists (open or closed)
    // for this branch.  If a closed PR exists, reopen it instead of creating
    // a duplicate — this preserves the review conversation and avoids PR churn.
    let existing_pr = match github_client
        .list_pulls_by_head_with_state(&owner, &repo_name, &head_ref, "all")
        .await
    {
        Ok(prs) => prs.into_iter().next(),
        Err(e) => {
            tracing::warn!(
                task_id = %task_id,
                error = %e,
                "Failed to check for existing PRs, will attempt to create new"
            );
            None
        }
    };

    let pr = if let Some(existing) = existing_pr {
        if existing.state == PrState::Open {
            tracing::info!(
                task_id = %task_id,
                pr_url = %existing.html_url,
                pr_number = existing.number,
                "Lifecycle: adopting existing open GitHub PR"
            );
            existing
        } else {
            // Closed PR — reopen it.
            match github_client
                .reopen_pull_request(&owner, &repo_name, existing.number)
                .await
            {
                Ok(reopened) => {
                    tracing::info!(
                        task_id = %task_id,
                        pr_url = %reopened.html_url,
                        pr_number = reopened.number,
                        "Lifecycle: reopened existing closed GitHub PR"
                    );
                    reopened
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = %task_id,
                        pr_number = existing.number,
                        error = %e,
                        "Failed to reopen closed PR, creating new one"
                    );
                    github_client
                        .create_pull_request(
                            &owner,
                            &repo_name,
                            CreatePrParams {
                                title: pr_title,
                                body: pr_body,
                                head: base_branch.to_string(),
                                base: merge_target.to_string(),
                                maintainer_can_modify: Some(true),
                                draft: Some(true),
                            },
                        )
                        .await
                        .map_err(|e| format!("GitHub PR creation failed: {e}"))?
                }
            }
        }
    } else {
        github_client
            .create_pull_request(
                &owner,
                &repo_name,
                CreatePrParams {
                    title: pr_title,
                    body: pr_body,
                    head: base_branch.to_string(),
                    base: merge_target.to_string(),
                    maintainer_can_modify: Some(true),
                    draft: Some(true),
                },
            )
            .await
            .map_err(|e| format!("GitHub PR creation failed: {e}"))?
    };

    // Store the PR URL on the task record.
    if let Err(e) = task_repo.set_pr_url(task_id, &pr.html_url).await {
        tracing::warn!(
            task_id = %task_id,
            error = %e,
            "failed to store pr_url on task (non-fatal)"
        );
    }

    tracing::info!(
        task_id = %task.short_id,
        pr_url = %pr.html_url,
        pr_number = pr.number,
        "Lifecycle: GitHub PR created"
    );

    Ok(Some(pr.html_url))
}

/// Parse `owner` and `repo` from a GitHub remote URL.
///
/// Supports both HTTPS (`https://github.com/owner/repo.git`) and SSH
/// (`git@github.com:owner/repo.git`) formats.
///
/// Kept for tests and potential future use; the production push path now
/// reads coordinates from the `projects` DB row instead.
#[allow(dead_code)]
fn parse_github_owner_repo(remote_url: &str) -> Option<(String, String)> {
    // Normalize: strip user@ from HTTPS URLs (e.g. https://user@github.com/...)
    let url = if let Some(rest) = remote_url.strip_prefix("https://") {
        if let Some(at_pos) = rest.find('@') {
            format!("https://{}", &rest[at_pos + 1..])
        } else {
            remote_url.to_string()
        }
    } else if let Some(rest) = remote_url.strip_prefix("http://") {
        if let Some(at_pos) = rest.find('@') {
            format!("http://{}", &rest[at_pos + 1..])
        } else {
            remote_url.to_string()
        }
    } else {
        remote_url.to_string()
    };

    // SSH: git@github.com:owner/repo.git
    if let Some(path) = url.strip_prefix("git@github.com:") {
        return split_owner_repo(path);
    }
    // HTTPS: https://github.com/owner/repo.git
    for prefix in &["https://github.com/", "http://github.com/"] {
        if let Some(path) = url.strip_prefix(prefix) {
            return split_owner_repo(path);
        }
    }
    None
}

fn split_owner_repo(path: &str) -> Option<(String, String)> {
    let path = path.trim_end_matches(".git");
    let mut parts = path.splitn(2, '/');
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.to_string();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner, repo))
}

pub(crate) async fn merge_and_transition(
    task_id: &str,
    app_state: &AgentContext,
    actions: &MergeActions,
    verification_gate: Option<VerificationGateFn>,
) -> Option<(TransitionAction, Option<String>)> {
    let repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let task = match repo.get(task_id).await {
        Ok(Some(task)) => task,
        Ok(None) => {
            return Some((
                actions.release.clone(),
                Some("task missing during post-review merge".to_string()),
            ));
        }
        Err(e) => {
            return Some((
                actions.release.clone(),
                Some(format!("failed to load task for merge: {e}")),
            ));
        }
    };

    let project_dir = project_path_for_id(&task.project_id, app_state).await;

    let project_path_str = project_dir.to_string_lossy().to_string();
    let verification_result = match verification_gate {
        Some(gate) => gate(task_id.to_string(), project_path_str.clone()).await,
        None => Ok(()),
    };
    if let Err(feedback) = verification_result {
        tracing::warn!(
            task_id = %task_id,
            "pre-merge verification failed; routing to worker"
        );
        let payload = serde_json::json!({ "body": feedback }).to_string();
        let _ = repo
            .log_activity(
                Some(task_id),
                "agent-supervisor",
                "verification",
                "comment",
                &payload,
            )
            .await;
        // Use verification_fail action (→ worker) if available, otherwise fall
        // back to release. This avoids a reviewer loop where the reviewer keeps
        // approving but the code never gets fixed.
        let action = actions
            .verification_fail
            .as_ref()
            .unwrap_or(&actions.release)
            .clone();
        return Some((
            action,
            Some(format!("pre-merge verification failed: {feedback}")),
        ));
    }

    let base_branch = format!("task/{}", task.short_id);
    let merge_target = default_target_branch(&task.project_id, app_state).await;

    // ── GitHub App path: create PR instead of direct-push merge ──────────────
    match try_create_github_pr(
        task_id,
        &base_branch,
        &merge_target,
        &project_dir,
        &task.project_id,
        app_state,
    )
    .await
    {
        Ok(Some(pr_url)) => {
            // PR created and auto-merge enabled.  Under the mirror-native
            // dispatch path (task #8) the task never had a user-visible
            // worktree to tear down — the supervisor used an ephemeral
            // clone that was already dropped.  The task branch stays on
            // the remote for the PR to merge; GitHub cleans it up after
            // merge if "Automatically delete head branches" is enabled.
            let _ = apply_task_knowledge_decision(
                task_id,
                KnowledgePromotionDecision::Promote,
                KnowledgeCleanupReason::TaskCompleted,
                app_state,
            )
            .await;
            cleanup_paused_worker_session(task_id, app_state).await;
            let _ = repo
                .log_activity(
                    Some(task_id),
                    "agent-supervisor",
                    "system",
                    "pr_created",
                    &serde_json::json!({ "pr_url": pr_url }).to_string(),
                )
                .await;
            let action = actions
                .pr_created
                .as_ref()
                .unwrap_or(&actions.approve)
                .clone();
            return Some((action, None));
        }
        Ok(None) => {
            // No GitHub App credential — fall through to direct-push merge below.
        }
        Err(reason) if reason.starts_with(EMPTY_BRANCH_PREFIX) => {
            // Branch has no unique commits vs the merge target — the work was
            // already absorbed (e.g. prerequisite tasks merged separately).
            // Close the task and delete the task branch from the remote so
            // it doesn't linger.  No worktree teardown under the mirror-
            // native dispatch path (task #8).
            tracing::info!(
                task_id = %task.short_id,
                "Branch has no unique commits vs merge target; closing as already merged"
            );
            let base_branch_to_delete = format!("task/{}", task.short_id);
            if let Ok(git) = app_state.git_actor(&project_dir).await
                && let Err(e) = git.delete_branch(&base_branch_to_delete).await
            {
                tracing::warn!(
                    task_id = %task.short_id,
                    branch = %base_branch_to_delete,
                    error = %e,
                    "Lifecycle: failed to delete absorbed task branch"
                );
            }
            let _ = apply_task_knowledge_decision(
                task_id,
                KnowledgePromotionDecision::Discard,
                KnowledgeCleanupReason::TaskAbandoned,
                app_state,
            )
            .await;
            cleanup_paused_worker_session(task_id, app_state).await;
            let _ = repo
                .log_activity(
                    Some(task_id),
                    "agent-supervisor",
                    "system",
                    "pr_skipped_empty_branch",
                    &serde_json::json!({ "reason": reason }).to_string(),
                )
                .await;
            return Some((actions.approve.clone(), Some(reason)));
        }
        Err(reason) => {
            tracing::warn!(
                task_id = %task.short_id,
                error = %reason,
                "GitHub PR creation failed; escalating to lead intervention"
            );
            let _ = repo
                .log_activity(
                    Some(task_id),
                    "agent-supervisor",
                    "system",
                    "pr_creation_failed",
                    &serde_json::json!({ "reason": reason }).to_string(),
                )
                .await;
            // PR creation failure is an infra/auth problem, not a code problem.
            // Use pr_creation_fail action if set (→ NeedsLeadIntervention for the
            // reviewer path) to avoid looping the reviewer. Fall back to release
            // only when no dedicated action is configured (e.g. Lead path).
            let action = actions
                .pr_creation_fail
                .as_ref()
                .unwrap_or(&actions.release)
                .clone();
            return Some((action, Some(format!("GitHub PR creation failed: {reason}"))));
        }
    }

    // ── Direct-push merge fallback (no GitHub App) ────────────────────────────
    let commit_type = if task.issue_type == "task" {
        "chore"
    } else {
        "feat"
    };
    let message = format!("{}({}): {}", commit_type, task.short_id, task.title);

    let mirror = match app_state.mirror.as_ref() {
        Some(m) => m.clone(),
        None => {
            return Some((
                actions.release.clone(),
                Some(
                    "direct-push merge requires MirrorManager but AgentContext has none — \
                     configure the GitHub App or run on an AppState-backed context"
                        .to_string(),
                ),
            ));
        }
    };

    match squash_merge_via_mirror(
        mirror.as_ref(),
        &task.project_id,
        &base_branch,
        &merge_target,
        &message,
        None, // no installation token; push uses the mirror's origin URL
    )
    .await
    {
        Ok(result) => {
            tracing::info!(
                task_id = %task.short_id,
                task_uuid = %task.id,
                base_branch = %base_branch,
                merge_target = %merge_target,
                commit_sha = %result.commit_sha,
                "Lifecycle: post-review squash merge succeeded"
            );
            if let Err(e) = repo.set_merge_commit_sha(task_id, &result.commit_sha).await {
                return Some((
                    actions.release.clone(),
                    Some(format!("merged but failed to store merge SHA: {e}")),
                ));
            }
            // Delete the task branch from the local clone — it's been squashed
            // onto the target branch and pushed.  Under the mirror-native
            // dispatch path (task #8) there's no worktree to tear down; the
            // supervisor used an ephemeral workspace that was already dropped.
            let base_branch_to_delete = format!("task/{}", task.short_id);
            if let Ok(git) = app_state.git_actor(&project_dir).await
                && let Err(e) = git.delete_branch(&base_branch_to_delete).await
            {
                tracing::warn!(
                    task_id = %task.short_id,
                    branch = %base_branch_to_delete,
                    error = %e,
                    "Lifecycle: failed to delete merged task branch"
                );
            }
            let _ = apply_task_knowledge_decision(
                task_id,
                KnowledgePromotionDecision::Promote,
                KnowledgeCleanupReason::TaskCompleted,
                app_state,
            )
            .await;
            cleanup_paused_worker_session(task_id, app_state).await;
            Some((actions.approve.clone(), None))
        }
        Err(GitError::MergeConflict { files, .. }) => {
            tracing::warn!(
                task_id = %task.short_id,
                task_uuid = %task.id,
                conflict_count = files.len(),
                conflicting_files = ?files,
                "Lifecycle: post-review merge conflict"
            );
            let metadata = MergeConflictMetadata {
                conflicting_files: files,
                base_branch,
                merge_target,
            };
            let reason = match serde_json::to_string(&metadata) {
                Ok(v) => format!("{MERGE_CONFLICT_PREFIX}{v}"),
                Err(_) => format!("{MERGE_CONFLICT_PREFIX}{{}}"),
            };
            let payload = serde_json::to_string(&metadata).unwrap_or_else(|_| "{}".to_string());
            let _ = repo
                .log_activity(
                    Some(task_id),
                    "agent-supervisor",
                    "system",
                    "merge_conflict",
                    &payload,
                )
                .await;
            Some((actions.conflict.clone(), Some(reason)))
        }
        Err(GitError::CommitRejected {
            code,
            command,
            cwd,
            stdout,
            stderr,
        }) => {
            tracing::warn!(
                task_id = %task.short_id,
                exit_code = code,
                command = %command,
                "Lifecycle: post-review merge commit rejected"
            );
            let metadata = MergeValidationFailureMetadata {
                base_branch,
                merge_target,
                command,
                cwd,
                exit_code: code,
                stdout,
                stderr,
            };
            let reason_payload =
                serde_json::to_string(&metadata).unwrap_or_else(|_| "{}".to_string());
            let reason = format!("{MERGE_VALIDATION_PREFIX}{reason_payload}");
            let _ = repo
                .log_activity(
                    Some(task_id),
                    "agent-supervisor",
                    "system",
                    "merge_validation_failed",
                    &reason_payload,
                )
                .await;
            Some((actions.conflict.clone(), Some(reason)))
        }
        Err(e) => {
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                "Lifecycle: post-review squash merge failed"
            );
            Some((
                actions.release.clone(),
                Some(format!("post-review squash merge failed: {e} ({e:?})")),
            ))
        }
    }
}

pub(crate) async fn cleanup_paused_worker_session(task_id: &str, app_state: &AgentContext) {
    let repo = SessionRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let Ok(Some(paused)) = repo.paused_for_task(task_id).await else {
        return;
    };

    if let Err(e) = repo
        .update(
            &paused.id,
            SessionStatus::Completed,
            paused.tokens_in,
            paused.tokens_out,
        )
        .await
    {
        tracing::warn!(
            record_id = %paused.id,
            error = %e,
            "failed to finalize paused session record on task approval"
        );
    }

    // Task #8: paused worker sessions from the legacy lifecycle used to
    // persist `.djinn/worktrees/<short_id>` directories alongside the DB
    // record.  The supervisor-driven path doesn't create those, so there's
    // nothing to remove from the filesystem — the session row is enough.
    // Any stale directories from pre-migration runs are harvested by the
    // coordinator's `sweep_stale_resources` GC on the next tick.
    let _ = paused;
}

pub(crate) async fn interrupt_paused_worker_session(task_id: &str, app_state: &AgentContext) {
    let repo = SessionRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let Ok(Some(paused)) = repo.paused_for_task(task_id).await else {
        return;
    };

    if let Err(e) = repo
        .update(
            &paused.id,
            SessionStatus::Interrupted,
            paused.tokens_in,
            paused.tokens_out,
        )
        .await
    {
        tracing::warn!(
            task_id = %task_id,
            record_id = %paused.id,
            error = %e,
            "failed to interrupt paused worker session after reviewer rejection"
        );
    }
}

pub(crate) async fn resolve_project_path_for_id(
    project_id: &str,
    app_state: &AgentContext,
) -> Option<String> {
    let repo = ProjectRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    repo.get_path(project_id).await.ok().flatten()
}

async fn default_target_branch(project_id: &str, app_state: &AgentContext) -> String {
    let repo = ProjectRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    if let Ok(Some(config)) = repo.get_config(project_id).await {
        return config.target_branch;
    }
    "main".to_string()
}

async fn project_path_for_id(project_id: &str, app_state: &AgentContext) -> PathBuf {
    let project_path = resolve_project_path_for_id(project_id, app_state)
        .await
        .unwrap_or_else(|| ".".to_string());
    PathBuf::from(project_path)
}

/// Mirror-native squash-merge of `branch` into `target_branch`.
///
/// Replaces the legacy `.djinn/worktrees/.rebase-*` + `.djinn/worktrees/.merge-*`
/// tempo worktrees under the user's project clone. Steps:
///
/// 1. `MirrorManager::clone_ephemeral(project_id, target_branch)` produces a
///    hardlinked, tempdir-backed `Workspace` with `origin` pointed at the
///    bare mirror.
/// 2. `git fetch origin {branch}` brings the task branch into the workspace
///    as `origin/{branch}`.
/// 3. The task branch is checked out, rebased onto `origin/{target_branch}`,
///    and merged back into the target with `git merge --squash`.
/// 4. The squashed commit is pushed back to the real remote. If
///    `installation_push_url` is supplied, it is used verbatim (production
///    GitHub App flow); otherwise the workspace pushes to its `origin`
///    (the mirror), which is the fallback for deployments without the App.
///
/// The workspace is dropped at the end of the call — `TempDir` cleans up the
/// local copy, and the object db was hardlinked from the mirror so nothing
/// is left behind.
pub(crate) async fn squash_merge_via_mirror(
    mirror: &MirrorManager,
    project_id: &str,
    branch: &str,
    target_branch: &str,
    message: &str,
    installation_push_url: Option<&str>,
) -> Result<MergeResult, GitError> {
    // 1. Clone ephemeral workspace on the target branch.
    let workspace = mirror
        .clone_ephemeral(project_id, target_branch)
        .await
        .map_err(|e| GitError::Other(anyhow::anyhow!("clone_ephemeral: {e}")))?;
    let wt = workspace.path_buf();

    // Apply the bot identity locally so any commit we create is attributed to
    // the App (mirrors the worktree-era `user.name`/`user.email` seed).
    let (bot_name, bot_email) = bot_identity();
    let _ = djinn_git::run_git_command(
        wt.clone(),
        vec![
            "config".into(),
            "user.name".into(),
            bot_name.clone(),
        ],
    )
    .await;
    let _ = djinn_git::run_git_command(
        wt.clone(),
        vec![
            "config".into(),
            "user.email".into(),
            bot_email.clone(),
        ],
    )
    .await;

    // 2. Fetch the task branch from the mirror so we can reference it.
    djinn_git::run_git_command(
        wt.clone(),
        vec![
            "fetch".into(),
            "origin".into(),
            format!("{branch}:refs/remotes/origin/{branch}"),
        ],
    )
    .await?;

    // 3a. Check out the task branch, rebase onto origin/{target_branch}.
    djinn_git::run_git_command(
        wt.clone(),
        vec![
            "checkout".into(),
            "-B".into(),
            branch.to_string(),
            format!("origin/{branch}"),
        ],
    )
    .await?;

    let origin_target = format!("origin/{target_branch}");
    let rebase_ok = djinn_git::run_git_command(
        wt.clone(),
        vec!["rebase".into(), origin_target.clone()],
    )
    .await
    .is_ok();
    if !rebase_ok {
        let _ =
            djinn_git::run_git_command(wt.clone(), vec!["rebase".into(), "--abort".into()]).await;
    }

    // 3b. Switch to a detached copy of origin/{target_branch} and squash-merge.
    djinn_git::run_git_command(
        wt.clone(),
        vec![
            "checkout".into(),
            "--detach".into(),
            origin_target.clone(),
        ],
    )
    .await?;

    if let Err(err) = djinn_git::run_git_command(
        wt.clone(),
        vec!["merge".into(), "--squash".into(), branch.to_string()],
    )
    .await
    {
        if matches!(err, GitError::CommandFailed { .. }) {
            let files = djinn_git::unmerged_files(wt.clone()).await.unwrap_or_default();
            let _ =
                djinn_git::run_git_command(wt.clone(), vec!["merge".into(), "--abort".into()])
                    .await;
            if !files.is_empty() {
                return Err(GitError::MergeConflict {
                    target_branch: target_branch.to_string(),
                    files,
                });
            }
        }
        return Err(err);
    }

    let staged = djinn_git::run_git_command(
        wt.clone(),
        vec!["diff".into(), "--cached".into(), "--name-only".into()],
    )
    .await?;
    if staged.stdout.trim().is_empty() {
        // Nothing new to commit — the task branch was already absorbed into
        // the target. Return the target's HEAD as the "merge" commit.
        let out = djinn_git::run_git_command(
            wt.clone(),
            vec!["rev-parse".into(), "HEAD".into()],
        )
        .await?;
        return Ok(MergeResult {
            commit_sha: out.stdout.trim().to_string(),
        });
    }

    match djinn_git::run_git_command(
        wt.clone(),
        vec!["commit".into(), "-m".into(), message.to_string()],
    )
    .await
    {
        Ok(_) => {}
        Err(GitError::CommandFailed {
            code,
            command,
            cwd,
            stdout,
            stderr,
        }) => {
            return Err(GitError::CommitRejected {
                code,
                command,
                cwd,
                stdout,
                stderr,
            });
        }
        Err(e) => return Err(e),
    }

    let out = djinn_git::run_git_command(
        wt.clone(),
        vec!["rev-parse".into(), "HEAD".into()],
    )
    .await?;
    let commit_sha = out.stdout.trim().to_string();

    // 4. Push the squashed commit to the real remote. The workspace's `origin`
    //    points at the mirror, not GitHub — when an installation token is
    //    supplied we override the push URL to GitHub directly; otherwise we
    //    fall back to pushing to the mirror (which is the only sensible target
    //    in no-App deployments).
    let push_refspec = format!("{commit_sha}:refs/heads/{target_branch}");
    let mut push_args: Vec<String> = vec!["push".into()];
    match installation_push_url {
        Some(url) => push_args.push(url.to_string()),
        None => push_args.push("origin".into()),
    };
    push_args.push(push_refspec);

    let mut last_push_error: Option<GitError> = None;
    for attempt in 1..=djinn_git::PUSH_MAX_ATTEMPTS {
        match djinn_git::run_git_command(wt.clone(), push_args.clone()).await {
            Ok(_) => {
                last_push_error = None;
                break;
            }
            Err(e)
                if attempt < djinn_git::PUSH_MAX_ATTEMPTS
                    && djinn_git::is_retryable_git_command_error(&e) =>
            {
                last_push_error = Some(e);
                tokio::time::sleep(djinn_git::retry_delay(attempt)).await;
            }
            Err(e) => return Err(e),
        }
    }
    if let Some(e) = last_push_error {
        return Err(e);
    }

    Ok(MergeResult { commit_sha })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_app_push_url_shape() {
        let url = build_app_push_url("acme", "widgets", "ghs_FAKETOKEN123");
        assert_eq!(
            url,
            "https://x-access-token:ghs_FAKETOKEN123@github.com/acme/widgets.git"
        );
    }

    #[test]
    fn build_app_push_url_strips_trailing_dot_git() {
        let url = build_app_push_url("acme", "widgets.git", "ghs_tok");
        assert_eq!(
            url,
            "https://x-access-token:ghs_tok@github.com/acme/widgets.git"
        );
    }

    #[test]
    fn build_app_push_url_uses_x_access_token_user() {
        let url = build_app_push_url("octo", "hello-world", "ghs_abc");
        assert!(url.starts_with("https://x-access-token:"));
        assert!(url.contains("@github.com/octo/hello-world.git"));
        // Must never fall back to `x-oauth-basic` (the legacy PAT form).
        assert!(!url.contains("x-oauth-basic"));
    }

    #[test]
    fn parse_ssh_remote() {
        let (owner, repo) = parse_github_owner_repo("git@github.com:acme/widgets.git").unwrap();
        assert_eq!(owner, "acme");
        assert_eq!(repo, "widgets");
    }

    #[test]
    fn parse_https_remote() {
        let (owner, repo) = parse_github_owner_repo("https://github.com/acme/widgets.git").unwrap();
        assert_eq!(owner, "acme");
        assert_eq!(repo, "widgets");
    }

    #[test]
    fn parse_https_without_dot_git() {
        let (owner, repo) = parse_github_owner_repo("https://github.com/acme/widgets").unwrap();
        assert_eq!(owner, "acme");
        assert_eq!(repo, "widgets");
    }

    #[test]
    fn parse_https_with_user_prefix() {
        let (owner, repo) = parse_github_owner_repo(
            "https://CroosALt@github.com/getalternative/svc-accounts-payable.git",
        )
        .unwrap();
        assert_eq!(owner, "getalternative");
        assert_eq!(repo, "svc-accounts-payable");
    }

    #[test]
    fn parse_https_with_user_prefix_no_dot_git() {
        let (owner, repo) =
            parse_github_owner_repo("https://user@github.com/acme/widgets").unwrap();
        assert_eq!(owner, "acme");
        assert_eq!(repo, "widgets");
    }

    #[test]
    fn parse_http_with_user_prefix() {
        let (owner, repo) =
            parse_github_owner_repo("http://user@github.com/acme/widgets.git").unwrap();
        assert_eq!(owner, "acme");
        assert_eq!(repo, "widgets");
    }

    #[test]
    fn parse_non_github_returns_none() {
        assert!(parse_github_owner_repo("git@gitlab.com:acme/widgets.git").is_none());
        assert!(parse_github_owner_repo("https://gitlab.com/acme/widgets.git").is_none());
        assert!(parse_github_owner_repo("https://user@gitlab.com/acme/widgets.git").is_none());
    }

    #[test]
    fn parse_empty_owner_or_repo_returns_none() {
        assert!(parse_github_owner_repo("git@github.com:/widgets.git").is_none());
        assert!(parse_github_owner_repo("git@github.com:acme/").is_none());
    }
}
