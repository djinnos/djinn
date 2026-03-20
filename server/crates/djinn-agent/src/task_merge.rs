use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use crate::context::AgentContext;
use djinn_core::models::{SessionStatus, TransitionAction};
use djinn_db::{ProjectRepository, SessionRepository, TaskRepository};
use djinn_git::GitError;
use djinn_provider::github_api::{CreatePrParams, GitHubApiClient, MergeMethod};
use djinn_provider::oauth::github_app::GITHUB_APP_OAUTH_DB_KEY;
use djinn_provider::repos::CredentialRepository;

const MERGE_CONFLICT_PREFIX: &str = "merge_conflict:";
const MERGE_VALIDATION_PREFIX: &str = "merge_validation_failed:";

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
/// Allows both the reviewer and PM approval paths to reuse the same merge logic.
pub(crate) struct MergeActions {
    pub(crate) approve: TransitionAction,
    pub(crate) conflict: TransitionAction,
    pub(crate) release: TransitionAction,
}

/// Standard actions used by the task reviewer path.
pub(crate) const REVIEWER_MERGE_ACTIONS: MergeActions = MergeActions {
    approve: TransitionAction::TaskReviewApprove,
    conflict: TransitionAction::TaskReviewRejectConflict,
    release: TransitionAction::ReleaseTaskReview,
};

/// Actions used when Lead approves directly from intervention.
///
/// `release` uses `LeadInterventionComplete` (→ Open) instead of
/// `LeadInterventionRelease` (→ NeedsLeadIntervention) so that verification
/// or git failures route the task back to a worker who can fix the code,
/// rather than looping the Lead in a re-dispatch cycle it cannot resolve.
pub(crate) const PM_MERGE_ACTIONS: MergeActions = MergeActions {
    approve: TransitionAction::LeadApprove,
    conflict: TransitionAction::LeadApproveConflict,
    release: TransitionAction::LeadInterventionComplete,
};

pub(crate) async fn merge_after_task_review(
    task_id: &str,
    app_state: &AgentContext,
    verification_gate: Option<VerificationGateFn>,
) -> Option<(TransitionAction, Option<String>)> {
    merge_and_transition(
        task_id,
        app_state,
        &REVIEWER_MERGE_ACTIONS,
        verification_gate,
    )
    .await
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
    app_state: &AgentContext,
) -> Result<Option<String>, String> {
    let cred_repo = Arc::new(CredentialRepository::new(
        app_state.db.clone(),
        app_state.event_bus.clone(),
    ));

    // Check whether the GitHub App credential exists.
    let has_app = cred_repo
        .get_decrypted(GITHUB_APP_OAUTH_DB_KEY)
        .await
        .ok()
        .flatten()
        .is_some();
    if !has_app {
        return Ok(None);
    }

    // Resolve owner/repo from `git remote get-url origin`.
    let remote_output = djinn_git::run_git_command(
        project_dir.to_path_buf(),
        vec!["remote".into(), "get-url".into(), "origin".into()],
    )
    .await
    .map_err(|e| format!("failed to get git remote URL: {e}"))?;
    let remote_url = remote_output.stdout.trim().to_string();
    let (owner, repo_name) = parse_github_owner_repo(&remote_url)
        .ok_or_else(|| format!("could not parse GitHub owner/repo from remote: {remote_url}"))?;

    // Push the branch to origin before opening the PR.
    djinn_git::run_git_command(
        project_dir.to_path_buf(),
        vec![
            "push".into(),
            "--force-with-lease".into(),
            "origin".into(),
            format!("{base_branch}:{base_branch}"),
        ],
    )
    .await
    .map_err(|e| format!("failed to push branch {base_branch} to origin: {e}"))?;

    // Load the task for PR body construction.
    let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let task = task_repo
        .get(task_id)
        .await
        .ok()
        .flatten()
        .ok_or_else(|| "task not found for PR body".to_string())?;

    // Build diff stat via `git diff --stat origin/{merge_target}..{base_branch}`.
    let diff_stat = djinn_git::run_git_command(
        project_dir.to_path_buf(),
        vec![
            "diff".into(),
            "--stat".into(),
            format!("origin/{merge_target}..{base_branch}"),
        ],
    )
    .await
    .map(|o| o.stdout.trim().to_string())
    .unwrap_or_else(|_| String::new());

    // Build acceptance criteria checklist.
    let criteria_lines: String = if task.acceptance_criteria.trim().is_empty() {
        String::new()
    } else {
        task.acceptance_criteria
            .lines()
            .map(|l| {
                let l = l.trim().trim_start_matches('-').trim();
                format!("- [ ] {l}\n")
            })
            .collect()
    };

    let pr_body = format!(
        "## Summary\n{description}\n\n## Acceptance Criteria\n{criteria}## Files Changed\n```\n{diff_stat}\n```\n\n---\nDjinn task: {short_id}",
        description = task.description,
        criteria = criteria_lines,
        diff_stat = diff_stat,
        short_id = task.short_id,
    );

    let commit_type = if task.issue_type == "task" {
        "chore"
    } else {
        "feat"
    };
    let pr_title = format!("{}({}): {}", commit_type, task.short_id, task.title);

    let github_client = GitHubApiClient::new(cred_repo);
    let pr = github_client
        .create_pull_request(
            &owner,
            &repo_name,
            CreatePrParams {
                title: pr_title,
                body: pr_body,
                head: base_branch.to_string(),
                base: merge_target.to_string(),
                maintainer_can_modify: Some(true),
                draft: Some(false),
            },
        )
        .await
        .map_err(|e| format!("GitHub PR creation failed: {e}"))?;

    // Enable auto-merge (best-effort — log failure but don't block).
    if let Err(e) = github_client
        .enable_auto_merge(&owner, &repo_name, pr.number, MergeMethod::Squash)
        .await
    {
        tracing::warn!(
            pr_number = pr.number,
            error = %e,
            "enable_auto_merge failed (non-fatal)"
        );
    }

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
fn parse_github_owner_repo(remote_url: &str) -> Option<(String, String)> {
    // SSH: git@github.com:owner/repo.git
    if let Some(path) = remote_url.strip_prefix("git@github.com:") {
        return split_owner_repo(path);
    }
    // HTTPS: https://github.com/owner/repo.git
    for prefix in &["https://github.com/", "http://github.com/"] {
        if let Some(path) = remote_url.strip_prefix(prefix) {
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
    let git = match app_state.git_actor(&project_dir).await {
        Ok(git) => git,
        Err(e) => {
            return Some((
                actions.release.clone(),
                Some(format!("failed to open git actor for merge: {e}")),
            ));
        }
    };

    let project_path_str = project_dir.to_string_lossy().to_string();
    let verification_result = match verification_gate {
        Some(gate) => gate(task_id.to_string(), project_path_str.clone()).await,
        None => Ok(()),
    };
    if let Err(feedback) = verification_result {
        tracing::warn!(
            task_id = %task_id,
            "pre-merge verification failed; releasing task"
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
        return Some((
            actions.release.clone(),
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
        app_state,
    )
    .await
    {
        Ok(Some(pr_url)) => {
            // PR created and auto-merge enabled. Teardown worktree and approve.
            let worktree_path = project_dir
                .join(".djinn")
                .join("worktrees")
                .join(&task.short_id);
            crate::actors::slot::teardown_worktree(
                &task.short_id,
                &worktree_path,
                &project_dir,
                app_state,
                true,
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
            return Some((actions.approve.clone(), None));
        }
        Ok(None) => {
            // No GitHub App credential — fall through to direct-push merge below.
        }
        Err(reason) => {
            tracing::warn!(
                task_id = %task.short_id,
                error = %reason,
                "GitHub PR creation failed; releasing task"
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
            return Some((
                actions.release.clone(),
                Some(format!("GitHub PR creation failed: {reason}")),
            ));
        }
    }

    // ── Direct-push merge fallback (no GitHub App) ────────────────────────────
    let commit_type = if task.issue_type == "task" {
        "chore"
    } else {
        "feat"
    };
    let message = format!("{}({}): {}", commit_type, task.short_id, task.title);

    match git
        .squash_merge(&base_branch, &merge_target, &message)
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
            // Tear down in correct order: LSP → worktree dir → branch (branch deletion
            // always failed before because the worktree still held a ref to it).
            let worktree_path = project_dir
                .join(".djinn")
                .join("worktrees")
                .join(&task.short_id);
            crate::actors::slot::teardown_worktree(
                &task.short_id,
                &worktree_path,
                &project_dir,
                app_state,
                true,
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

    if let Some(worktree_str) = paused.worktree_path.as_deref() {
        let worktree_path = PathBuf::from(worktree_str);
        // Use teardown_worktree for correct ordering (LSP shutdown before dir removal).
        // Look up task + project path for the full teardown; fall back to raw remove if
        // the lookup fails.
        if let Some(project_path_str) = resolve_project_path_for_task(task_id, app_state).await {
            crate::actors::slot::teardown_worktree(
                &project_path_str.0,
                &worktree_path,
                Path::new(&project_path_str.1),
                app_state,
                false,
            )
            .await;
        } else {
            let _ = tokio::fs::remove_dir_all(&worktree_path).await;
        }
    }
}

/// Returns `(task_short_id, project_path_str)` for the given task UUID, or `None` if either
/// lookup fails.  Used by `cleanup_paused_worker_session` to supply arguments to
/// `teardown_worktree`.
async fn resolve_project_path_for_task(
    task_id: &str,
    app_state: &AgentContext,
) -> Option<(String, String)> {
    let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let task = task_repo.get(task_id).await.ok().flatten()?;
    let project_path = resolve_project_path_for_id(&task.project_id, app_state).await?;
    Some((task.short_id, project_path))
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
