use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::{debug, warn};

const MAX_PUSH_ATTEMPTS: usize = 3;
const INITIAL_RETRY_BACKOFF: Duration = Duration::from_millis(150);
const MAX_CHECKPOINT_FILE_BYTES: u64 = 50 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct CheckpointAuthor {
    pub name: String,
    pub email: String,
}

#[derive(Debug, Clone)]
pub struct CheckpointMetadata {
    pub session_id: String,
    pub turn: Option<u64>,
    pub reason: String,
}

impl CheckpointMetadata {
    pub fn message(&self) -> String {
        format!(
            "wip: checkpoint session {} turn {} reason {}",
            self.session_id,
            self.turn.unwrap_or(0),
            self.reason
        )
    }

    fn turn_for_ref(&self) -> u64 {
        self.turn.unwrap_or(0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointPushResult {
    NotAttempted,
    Pushed,
    PushedAlternateRef,
    NoChanges,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointConflictStrategy {
    None,
    SafeRefreshRebase,
    AlternateCheckpointRef,
    BlockedBySafetyScan,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointPreservationResult {
    pub commit_sha: Option<String>,
    pub parent_sha: Option<String>,
    pub local_sha: Option<String>,
    pub remote_sha: Option<String>,
    pub target_ref: Option<String>,
    pub retry_count: usize,
    pub conflict_strategy: CheckpointConflictStrategy,
    pub push_result: CheckpointPushResult,
    pub failure_reason: Option<String>,
    pub session_id: String,
    pub turn: Option<u64>,
    pub reason: String,
    pub safety: CheckpointSafetySummary,
}

impl CheckpointPreservationResult {
    fn failed(
        metadata: &CheckpointMetadata,
        failure_reason: String,
        conflict_strategy: CheckpointConflictStrategy,
        safety: CheckpointSafetySummary,
    ) -> Self {
        Self {
            commit_sha: None,
            parent_sha: None,
            local_sha: None,
            remote_sha: None,
            target_ref: None,
            retry_count: 0,
            conflict_strategy,
            push_result: CheckpointPushResult::Failed,
            failure_reason: Some(failure_reason),
            session_id: metadata.session_id.clone(),
            turn: metadata.turn,
            reason: metadata.reason.clone(),
            safety,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CheckpointSafetySummary {
    pub included_paths: Vec<String>,
    pub excluded_paths: Vec<CheckpointPathDecision>,
    pub blocked_paths: Vec<CheckpointPathDecision>,
}

impl CheckpointSafetySummary {
    fn is_blocked(&self) -> bool {
        !self.blocked_paths.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointPathDecision {
    pub path: String,
    pub reason: String,
}

#[derive(Debug)]
struct GitCommandOutput {
    stdout: String,
}

pub async fn preserve_checkpoint(
    workspace_path: &Path,
    task_branch: &str,
    author: &CheckpointAuthor,
    metadata: &CheckpointMetadata,
) -> CheckpointPreservationResult {
    let mut result = CheckpointPreservationResult {
        commit_sha: None,
        parent_sha: git_output(workspace_path, &["rev-parse", "HEAD"])
            .await
            .ok(),
        local_sha: None,
        remote_sha: remote_sha(workspace_path, task_branch).await,
        target_ref: Some(format!("refs/heads/{task_branch}")),
        retry_count: 0,
        conflict_strategy: CheckpointConflictStrategy::None,
        push_result: CheckpointPushResult::NotAttempted,
        failure_reason: None,
        session_id: metadata.session_id.clone(),
        turn: metadata.turn,
        reason: metadata.reason.clone(),
        safety: CheckpointSafetySummary::default(),
    };

    let safety = match safety_scan(workspace_path).await {
        Ok(safety) => safety,
        Err(e) => {
            return CheckpointPreservationResult::failed(
                metadata,
                format!("safety scan failed: {e}"),
                CheckpointConflictStrategy::BlockedBySafetyScan,
                CheckpointSafetySummary::default(),
            );
        }
    };
    result.safety = safety.clone();

    if safety.is_blocked() {
        return CheckpointPreservationResult::failed(
            metadata,
            format!("safety scan blocked {} path(s)", safety.blocked_paths.len()),
            CheckpointConflictStrategy::BlockedBySafetyScan,
            safety,
        );
    }

    if let Err(e) = unstage_all(workspace_path).await {
        return fail_existing(
            result,
            format!("failed to reset index before checkpoint: {e}"),
        );
    }

    if !safety.included_paths.is_empty()
        && let Err(e) = stage_paths(workspace_path, &safety.included_paths).await
    {
        return fail_existing(
            result,
            format!("failed to stage safety-approved paths: {e}"),
        );
    }

    let staged = git_output(workspace_path, &["diff", "--cached", "--name-only"])
        .await
        .unwrap_or_default();
    if !staged.trim().is_empty() {
        let message = metadata.message();
        if let Err(e) = git_output_with_env(
            workspace_path,
            &["commit", "-m", &message],
            &[
                ("GIT_AUTHOR_NAME", author.name.as_str()),
                ("GIT_AUTHOR_EMAIL", author.email.as_str()),
                ("GIT_COMMITTER_NAME", author.name.as_str()),
                ("GIT_COMMITTER_EMAIL", author.email.as_str()),
            ],
        )
        .await
        {
            return fail_existing(result, format!("WIP commit failed: {e}"));
        }
    } else if safety.included_paths.is_empty() {
        result.push_result = CheckpointPushResult::NoChanges;
    }

    result.commit_sha = git_output(workspace_path, &["rev-parse", "HEAD"])
        .await
        .ok();
    result.local_sha = result.commit_sha.clone();

    if result.local_sha.is_none() {
        return fail_existing(result, "failed to resolve local checkpoint sha".to_string());
    }

    push_with_lease_and_fallback(workspace_path, task_branch, metadata, result).await
}

async fn push_with_lease_and_fallback(
    workspace_path: &Path,
    task_branch: &str,
    metadata: &CheckpointMetadata,
    mut result: CheckpointPreservationResult,
) -> CheckpointPreservationResult {
    for attempt in 0..MAX_PUSH_ATTEMPTS {
        result.remote_sha = remote_sha(workspace_path, task_branch).await;
        match push_branch_with_lease(workspace_path, task_branch, result.remote_sha.as_deref())
            .await
        {
            Ok(()) => {
                result.retry_count = attempt;
                result.push_result = CheckpointPushResult::Pushed;
                result.target_ref = Some(format!("refs/heads/{task_branch}"));
                return result;
            }
            Err(e) if is_push_conflict(&e) => {
                result.retry_count = attempt + 1;
                if attempt + 1 >= MAX_PUSH_ATTEMPTS {
                    break;
                }
                if try_safe_refresh_rebase(workspace_path, task_branch).await {
                    result.conflict_strategy = CheckpointConflictStrategy::SafeRefreshRebase;
                    result.commit_sha = git_output(workspace_path, &["rev-parse", "HEAD"])
                        .await
                        .ok();
                    result.local_sha = result.commit_sha.clone();
                }
                tokio::time::sleep(INITIAL_RETRY_BACKOFF * (attempt as u32 + 1)).await;
            }
            Err(e) => return fail_existing(result, format!("checkpoint push failed: {e}")),
        }
    }

    result.conflict_strategy = CheckpointConflictStrategy::AlternateCheckpointRef;
    let Some(local_sha) = result.local_sha.clone() else {
        return fail_existing(
            result,
            "cannot push alternate checkpoint ref without local sha".to_string(),
        );
    };
    let checkpoint_ref = alternate_checkpoint_ref(task_branch, metadata, &local_sha);
    match push_sha_to_ref(workspace_path, &local_sha, &checkpoint_ref).await {
        Ok(()) => {
            result.target_ref = Some(checkpoint_ref);
            result.push_result = CheckpointPushResult::PushedAlternateRef;
            result.failure_reason = None;
            result
        }
        Err(e) => fail_existing(
            result,
            format!("checkpoint branch push conflicted and alternate ref push failed: {e}"),
        ),
    }
}

async fn try_safe_refresh_rebase(workspace_path: &Path, task_branch: &str) -> bool {
    if let Err(e) = git_output(workspace_path, &["fetch", "origin", task_branch]).await {
        warn!(branch = task_branch, error = %e, "checkpoint: refresh fetch failed");
        return false;
    }
    let remote_ref = format!("origin/{task_branch}");
    match git_output(
        workspace_path,
        &["merge-base", "--is-ancestor", &remote_ref, "HEAD"],
    )
    .await
    {
        Ok(_) => true,
        Err(_) => match git_output(workspace_path, &["rebase", &remote_ref]).await {
            Ok(_) => true,
            Err(e) => {
                warn!(branch = task_branch, error = %e, "checkpoint: safe refresh rebase failed; aborting and falling back if needed");
                let _ = git_output(workspace_path, &["rebase", "--abort"]).await;
                false
            }
        },
    }
}

async fn safety_scan(workspace_path: &Path) -> Result<CheckpointSafetySummary, String> {
    let status = git_output(workspace_path, &["status", "--porcelain=v1", "-z"]).await?;
    let mut summary = CheckpointSafetySummary::default();
    for path in parse_porcelain_paths(&status) {
        if let Some(reason) = exclusion_reason(&path) {
            summary
                .excluded_paths
                .push(CheckpointPathDecision { path, reason });
            continue;
        }
        if let Some(reason) = block_reason(workspace_path, &path).await {
            summary
                .blocked_paths
                .push(CheckpointPathDecision { path, reason });
            continue;
        }
        summary.included_paths.push(path);
    }
    summary.included_paths.sort();
    summary.included_paths.dedup();
    summary.excluded_paths.sort_by(|a, b| a.path.cmp(&b.path));
    summary.blocked_paths.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(summary)
}

fn parse_porcelain_paths(status: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut parts = status.split('\0').filter(|s| !s.is_empty());
    while let Some(entry) = parts.next() {
        if entry.len() < 4 {
            continue;
        }
        let code = &entry[..2];
        let path = entry[3..].to_string();
        if (code.contains('R') || code.contains('C'))
            && let Some(new_path) = parts.next()
        {
            out.push(new_path.to_string());
        }
        out.push(path);
    }
    out
}

fn exclusion_reason(path: &str) -> Option<String> {
    let normalized = path.replace('\\', "/");
    let components: Vec<&str> = normalized.split('/').collect();
    let excluded_dir = components.iter().any(|component| {
        matches!(
            *component,
            "target"
                | ".target"
                | "node_modules"
                | "__pycache__"
                | ".pytest_cache"
                | "dist"
                | "build"
                | ".cache"
                | "coverage"
                | ".next"
                | ".turbo"
        )
    });
    if excluded_dir {
        return Some("generated/cache/build output path excluded from checkpoint".to_string());
    }
    let lower = normalized.to_ascii_lowercase();
    if lower.ends_with(".log") || lower.ends_with(".lcov") || lower.ends_with(".profraw") {
        return Some("log/coverage artifact excluded from checkpoint".to_string());
    }
    None
}

async fn block_reason(workspace_path: &Path, path: &str) -> Option<String> {
    let full = workspace_path.join(path);
    let meta = match tokio::fs::symlink_metadata(&full).await {
        Ok(meta) => meta,
        Err(_) => return None,
    };
    if meta.file_type().is_dir() {
        return Some("submodule or nested worktree path blocked".to_string());
    }
    if !meta.file_type().is_file() {
        return None;
    }
    if meta.len() > MAX_CHECKPOINT_FILE_BYTES {
        return Some(format!(
            "file exceeds checkpoint safety limit ({} bytes > {} bytes)",
            meta.len(),
            MAX_CHECKPOINT_FILE_BYTES
        ));
    }
    if secret_like_path(path) {
        return Some("secret-like path blocked from checkpoint".to_string());
    }
    let sample_len = meta.len().min(8192) as usize;
    if sample_len == 0 {
        return None;
    }
    let bytes = match tokio::fs::read(&full).await {
        Ok(bytes) => bytes,
        Err(_) => return None,
    };
    let sample = &bytes[..bytes.len().min(sample_len)];
    if looks_binary(sample) {
        return Some("binary file blocked from checkpoint".to_string());
    }
    let text = String::from_utf8_lossy(sample);
    if secret_like_content(&text) {
        return Some("secret-like content blocked from checkpoint".to_string());
    }
    None
}

fn secret_like_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".pem")
        || lower.ends_with(".key")
        || lower.ends_with("id_rsa")
        || lower.ends_with("id_ed25519")
        || lower.contains(".env")
        || lower.contains("secret")
        || lower.contains("credentials")
}

fn secret_like_content(text: &str) -> bool {
    const NEEDLES: &[&str] = &[
        "BEGIN PRIVATE KEY",
        "BEGIN RSA PRIVATE KEY",
        "AWS_SECRET_ACCESS_KEY",
        "GITHUB_TOKEN=",
        "OPENAI_API_KEY=",
        "ANTHROPIC_API_KEY=",
        "api_key=",
        "access_token=",
    ];
    NEEDLES.iter().any(|needle| text.contains(needle))
}

fn looks_binary(bytes: &[u8]) -> bool {
    bytes.contains(&0)
}

async fn unstage_all(workspace_path: &Path) -> Result<(), String> {
    git_output(
        workspace_path,
        &["reset", "--mixed", "--quiet", "HEAD", "--"],
    )
    .await?;
    Ok(())
}

async fn stage_paths(workspace_path: &Path, paths: &[String]) -> Result<(), String> {
    let mut args = vec!["add", "--"];
    args.extend(paths.iter().map(String::as_str));
    git_output(workspace_path, &args).await?;
    Ok(())
}

async fn push_branch_with_lease(
    workspace_path: &Path,
    task_branch: &str,
    remote_sha: Option<&str>,
) -> Result<(), String> {
    if let Some(sha) = remote_sha {
        git_output(
            workspace_path,
            &["merge-base", "--is-ancestor", sha, "HEAD"],
        )
        .await
        .map_err(|e| format!("non-fast-forward checkpoint push requires refresh: {e}"))?;
    }
    let lease = match remote_sha {
        Some(sha) => format!("--force-with-lease=refs/heads/{task_branch}:{sha}"),
        None => format!("--force-with-lease=refs/heads/{task_branch}"),
    };
    let refspec = format!("{task_branch}:refs/heads/{task_branch}");
    git_output(workspace_path, &["push", &lease, "origin", &refspec]).await?;
    Ok(())
}

async fn push_sha_to_ref(workspace_path: &Path, sha: &str, target_ref: &str) -> Result<(), String> {
    let refspec = format!("{sha}:{target_ref}");
    git_output(workspace_path, &["push", "origin", &refspec]).await?;
    Ok(())
}

async fn remote_sha(workspace_path: &Path, task_branch: &str) -> Option<String> {
    let remote_ref = format!("refs/remotes/origin/{task_branch}");
    let _ = git_output(workspace_path, &["fetch", "origin", task_branch]).await;
    git_output(workspace_path, &["rev-parse", "--verify", &remote_ref])
        .await
        .ok()
}

fn alternate_checkpoint_ref(task_branch: &str, metadata: &CheckpointMetadata, sha: &str) -> String {
    let branch = sanitize_ref_component(task_branch);
    let session = sanitize_ref_component(&metadata.session_id);
    let short_sha = sha.get(..12).unwrap_or(sha);
    format!(
        "refs/djinn/checkpoints/{branch}/{session}/turn-{}/{}",
        metadata.turn_for_ref(),
        sanitize_ref_component(short_sha)
    )
}

fn sanitize_ref_component(input: &str) -> String {
    let sanitized: String = input
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = sanitized.trim_matches(['.', '-']).to_string();
    if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed
    }
}

fn is_push_conflict(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("stale info")
        || lower.contains("fetch first")
        || lower.contains("non-fast-forward")
        || lower.contains("rejected")
        || lower.contains("failed to push some refs")
}

fn fail_existing(
    mut result: CheckpointPreservationResult,
    failure_reason: String,
) -> CheckpointPreservationResult {
    result.push_result = CheckpointPushResult::Failed;
    result.failure_reason = Some(failure_reason);
    result
}

async fn git_output(workspace_path: &Path, args: &[&str]) -> Result<String, String> {
    git_output_with_env(workspace_path, args, &[]).await
}

async fn git_output_with_env(
    workspace_path: &Path,
    args: &[&str],
    env: &[(&str, &str)],
) -> Result<String, String> {
    let output = run_git(workspace_path, args, env).await?;
    Ok(output.stdout.trim().to_string())
}

async fn run_git(
    workspace_path: &Path,
    args: &[&str],
    env: &[(&str, &str)],
) -> Result<GitCommandOutput, String> {
    debug!(?args, path = %workspace_path.display(), "checkpoint git");
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(workspace_path).args(args);
    for (key, value) in env {
        cmd.env(key, value);
    }
    let output = cmd.output().await.map_err(|e| format!("spawn git: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}{}{}",
            args.join(" "),
            stderr.trim(),
            if stderr.trim().is_empty() { "" } else { " " },
            stdout.trim()
        )
        .trim()
        .to_string());
    }
    Ok(GitCommandOutput { stdout })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command as StdCommand;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = StdCommand::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn seed_repo() -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
        let origin = tempfile::TempDir::new().expect("origin");
        let op = origin.path();
        git(op, &["init", "--bare", "-b", "main"]);
        let clone = tempfile::TempDir::new().expect("clone");
        let cp = clone.path().to_path_buf();
        git(&cp, &["clone", op.to_str().unwrap(), "."]);
        std::fs::write(cp.join("base.txt"), "base\n").unwrap();
        git(&cp, &["add", "-A"]);
        git(&cp, &["commit", "-m", "base"]);
        git(&cp, &["push", "origin", "main"]);
        git(&cp, &["checkout", "-b", "task"]);
        git(&cp, &["push", "origin", "task"]);
        (origin, clone, cp)
    }

    fn author() -> CheckpointAuthor {
        CheckpointAuthor {
            name: "djinn-bot".to_string(),
            email: "bot@djinn.local".to_string(),
        }
    }

    fn metadata() -> CheckpointMetadata {
        CheckpointMetadata {
            session_id: "session-1".to_string(),
            turn: None,
            reason: "test".to_string(),
        }
    }

    #[test]
    fn wip_message_uses_explicit_unknown_turn_zero() {
        assert_eq!(
            metadata().message(),
            "wip: checkpoint session session-1 turn 0 reason test"
        );
    }

    #[tokio::test]
    async fn safety_block_prevents_commit_and_push() {
        let (origin, _clone, cp) = seed_repo();
        std::fs::write(cp.join(".env"), "OPENAI_API_KEY=secret\n").unwrap();

        let result = preserve_checkpoint(&cp, "task", &author(), &metadata()).await;

        assert_eq!(result.push_result, CheckpointPushResult::Failed);
        assert_eq!(
            result.conflict_strategy,
            CheckpointConflictStrategy::BlockedBySafetyScan
        );
        assert!(
            result
                .failure_reason
                .unwrap()
                .contains("safety scan blocked")
        );
        let remote = git(origin.path(), &["rev-parse", "task"]);
        let local_parent = git(&cp, &["rev-parse", "HEAD"]);
        assert_eq!(remote.trim(), local_parent.trim());
    }

    #[tokio::test]
    async fn checkpoint_pushes_wip_commit_with_lease() {
        let (origin, _clone, cp) = seed_repo();
        std::fs::write(cp.join("work.txt"), "in-flight\n").unwrap();

        let result = preserve_checkpoint(&cp, "task", &author(), &metadata()).await;

        assert_eq!(result.push_result, CheckpointPushResult::Pushed);
        assert_eq!(result.target_ref.as_deref(), Some("refs/heads/task"));
        let remote = git(origin.path(), &["rev-parse", "task"]);
        assert_eq!(Some(remote.trim()), result.commit_sha.as_deref());
        let message = git(&cp, &["log", "-1", "--pretty=%s"]);
        assert_eq!(
            message.trim(),
            "wip: checkpoint session session-1 turn 0 reason test"
        );
    }

    #[tokio::test]
    async fn conflicting_push_falls_back_to_checkpoint_ref() {
        let (origin, _clone, cp) = seed_repo();
        std::fs::write(cp.join("work.txt"), "local\n").unwrap();
        git(&cp, &["add", "-A"]);
        git(&cp, &["commit", "-m", "local divergent"]);

        let other = tempfile::TempDir::new().expect("other");
        let other_path = other.path();
        git(other_path, &["clone", origin.path().to_str().unwrap(), "."]);
        git(other_path, &["checkout", "task"]);
        std::fs::write(other_path.join("work.txt"), "remote\n").unwrap();
        git(other_path, &["add", "-A"]);
        git(other_path, &["commit", "-m", "remote divergent"]);
        git(other_path, &["push", "origin", "task"]);

        let result = preserve_checkpoint(&cp, "task", &author(), &metadata()).await;

        assert_eq!(result.push_result, CheckpointPushResult::PushedAlternateRef);
        assert_eq!(
            result.conflict_strategy,
            CheckpointConflictStrategy::AlternateCheckpointRef
        );
        let target_ref = result.target_ref.as_ref().expect("target ref");
        assert!(target_ref.starts_with("refs/djinn/checkpoints/task/session-1/turn-0/"));
        let remote_checkpoint = git(origin.path(), &["rev-parse", target_ref]);
        assert_eq!(Some(remote_checkpoint.trim()), result.commit_sha.as_deref());
    }
}
