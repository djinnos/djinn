//! Cross-cutting merge-safety tests for the resume-via-git and
//! PR-merge paths.
//!
//! Sibling task `8yjx` writes safety-scanned checkpoint commits to
//! `refs/djinn/checkpoints/...`; sibling `3ln4` selects among those refs
//! on re-dispatch. The tests in this module prove the final-merge
//! safety helpers added by sibling task `sy0g`:
//!
//! - [`djinn_workspace::MergeSafetyDecision`] /
//!   [`djinn_workspace::evaluate_merge_head`] — a checkpoint WIP commit
//!   cannot be fast-forwarded or directly merged to `main` as the
//!   accepted final result.
//! - [`djinn_workspace::is_checkpoint_ref`] /
//!   [`djinn_workspace::is_protected_ref`] — ref-shape guards the
//!   post-close branch cleanup and the merge head selector both
//!   consume.
//!
//! All tests run against a temporary bare mirror + a real git clone
//! and use no live GitHub credentials. They are intentionally
//! hermetic so CI can run them on every PR.

use std::path::Path;

use djinn_workspace::{
    CHECKPOINT_REF_PREFIX, MergeSafetyDecision, MirrorManager, RefRole, classify_ref,
    evaluate_merge_head, is_checkpoint_ref, is_protected_ref,
};
use tempfile::TempDir;
use tokio::process::Command;

async fn run(cmd: &[&str], cwd: &Path) {
    let output = Command::new(cmd[0])
        .args(&cmd[1..])
        .current_dir(cwd)
        .output()
        .await
        .expect("git spawn");
    assert!(
        output.status.success(),
        "cmd {cmd:?} failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn git_stdout(cmd: &[&str], cwd: &Path) -> String {
    let output = Command::new(cmd[0])
        .args(&cmd[1..])
        .current_dir(cwd)
        .output()
        .await
        .expect("git spawn");
    assert!(
        output.status.success(),
        "cmd {cmd:?} failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

async fn seed_source_repo(path: &Path) {
    run(&["git", "init", "-b", "main", "-q"], path).await;
    run(&["git", "config", "user.email", "test@example.com"], path).await;
    run(&["git", "config", "user.name", "Test"], path).await;
    tokio::fs::write(path.join("README.md"), "v1\n")
        .await
        .unwrap();
    run(&["git", "add", "."], path).await;
    run(&["git", "commit", "-m", "v1"], path).await;
}

/// Set up a mirror whose `main` branch has at least one commit, plus
/// a `task/<short_id>` branch with a second commit (simulating a
/// worker task run), plus an alternate checkpoint ref under
/// `refs/djinn/checkpoints/<task>/<sid>` pointing at the same
/// second commit. Returns the mirror manager + the SHA of the
/// worker's "task branch" commit (which is also the SHA the checkpoint
/// ref points at).
async fn build_mirror_with_task_branch_and_checkpoint(
    mirrors_dir: &Path,
    source_dir: &Path,
    short_id: &str,
) -> (MirrorManager, String, String) {
    let mgr = MirrorManager::new(mirrors_dir.to_path_buf());
    let project_id = "proj-merge-safety".to_string();
    let source_url = format!("file://{}", source_dir.display());
    mgr.ensure_mirror(&project_id, &source_url).await.unwrap();

    // Advance the source so there are two commits total. The second
    // commit is the worker's "task branch" / checkpoint candidate.
    tokio::fs::write(source_dir.join("worker.txt"), "checkpoint content\n")
        .await
        .unwrap();
    run(&["git", "add", "."], source_dir).await;
    run(&["git", "commit", "-m", "task work"], source_dir).await;

    // Lift the task branch into the mirror via a namespaced local ref so
    // we don't pollute `refs/heads/task/...` with every test's data.
    let tip = git_stdout(&["git", "rev-parse", "HEAD"], source_dir).await;
    let task_branch_ref = format!("refs/heads/task/{short_id}");
    run(
        &[
            "git",
            "push",
            &format!("file://{}", mgr.mirror_path(&project_id).display()),
            &format!("{tip}:{task_branch_ref}"),
        ],
        source_dir,
    )
    .await;

    // Also lift the same commit into an alternate checkpoint ref so the
    // merge-safety helpers have to distinguish "task branch with WIP
    // commit" from "checkpoint ref pointing at the same WIP commit".
    let alt_ref = format!("{CHECKPOINT_REF_PREFIX}task-{short_id}/session-1");
    run(
        &[
            "git",
            "push",
            &format!("file://{}", mgr.mirror_path(&project_id).display()),
            &format!("{tip}:{alt_ref}"),
        ],
        source_dir,
    )
    .await;

    (mgr, project_id, tip)
}

// ─── merge-head safety ──────────────────────────────────────────────────

/// Acceptance criterion 1 (sy0g): checkpoint WIP commits and alternate
/// checkpoint refs are classified as preservation/resume sources only
/// and are rejected by the final merge-to-main guard. The same SHA
/// reachable via the canonical task branch IS eligible.
#[tokio::test]
async fn checkpoint_ref_with_sha_is_rejected_from_final_merge() {
    let source_dir = TempDir::new().unwrap();
    seed_source_repo(source_dir.path()).await;
    let mirrors_dir = TempDir::new().unwrap();
    let (mgr, project_id, tip_sha) = build_mirror_with_task_branch_and_checkpoint(
        mirrors_dir.path(),
        source_dir.path(),
        "abc12",
    )
    .await;

    // The PR-open / squash-merge path would feed the same SHA into
    // `evaluate_merge_head` for the candidate head. The task branch
    // ref shape is Eligible; the checkpoint ref shape is rejected.
    let task_branch_decision =
        evaluate_merge_head("task-abc", "refs/heads/task/abc12", Some(&tip_sha));
    assert_eq!(
        task_branch_decision,
        MergeSafetyDecision::Eligible,
        "the same SHA on the canonical task branch ref shape must be eligible"
    );
    assert!(task_branch_decision.is_eligible());
    assert_eq!(task_branch_decision.rejection_tag(), "eligible");

    let checkpoint_ref = format!("{CHECKPOINT_REF_PREFIX}task-abc12/session-1");
    let checkpoint_decision = evaluate_merge_head("task-abc", &checkpoint_ref, Some(&tip_sha));
    assert!(
        !checkpoint_decision.is_eligible(),
        "the same SHA on a checkpoint ref shape MUST be rejected from the final merge"
    );
    match &checkpoint_decision {
        MergeSafetyDecision::CheckpointRef { ref_name, sha } => {
            assert_eq!(ref_name, &checkpoint_ref);
            assert_eq!(sha.as_deref(), Some(tip_sha.as_str()));
        }
        other => panic!("expected CheckpointRef, got {other:?}"),
    }
    assert_eq!(checkpoint_decision.rejection_tag(), "checkpoint_ref");

    // Both refs point at the same commit in the mirror — we confirm
    // that the safety distinction is purely ref-shape, not commit
    // content, by resolving both to the same SHA.
    let resolved_checkpoint_sha = git_stdout(
        &["git", "rev-parse", &checkpoint_ref],
        &mgr.mirror_path(&project_id),
    )
    .await;
    let resolved_task_branch_sha = git_stdout(
        &["git", "rev-parse", "refs/heads/task/abc12"],
        &mgr.mirror_path(&project_id),
    )
    .await;
    assert_eq!(
        resolved_checkpoint_sha, resolved_task_branch_sha,
        "fixture precondition: both refs must point at the same SHA"
    );
    assert_eq!(
        resolved_task_branch_sha, tip_sha,
        "fixture precondition: refs must match the source-dir tip"
    );
}

/// Acceptance criterion 2 (sy0g): even with a SHA, a checkpoint ref
/// cannot be the source of a fast-forward or direct merge into `main`.
/// The decision variant carries the SHA so the merge path can log it
/// in the structured rejection event for forensics.
#[tokio::test]
async fn checkpoint_sha_recorded_in_rejection_payload_for_forensics() {
    let source_dir = TempDir::new().unwrap();
    seed_source_repo(source_dir.path()).await;
    let mirrors_dir = TempDir::new().unwrap();
    let (_, _project_id, tip_sha) =
        build_mirror_with_task_branch_and_checkpoint(mirrors_dir.path(), source_dir.path(), "xyz9")
            .await;

    let checkpoint_ref = format!("{CHECKPOINT_REF_PREFIX}task-xyz9/session-1");
    let decision = evaluate_merge_head("task-xyz9", &checkpoint_ref, Some(&tip_sha));

    let sha = match decision {
        MergeSafetyDecision::CheckpointRef { sha, .. } => sha,
        other => panic!("expected CheckpointRef, got {other:?}"),
    };
    assert_eq!(
        sha.as_deref(),
        Some(tip_sha.as_str()),
        "rejection payload must carry the SHA the merge path would have used"
    );
}

// ─── cleanup safety ──────────────────────────────────────────────────────

/// Acceptance criterion 3 (sy0g): task branch / ref cleanup respects
/// protected branches and handles checkpoint refs explicitly without
/// accidentally deleting them. The bare-mirror precondition is set up
/// here; the actual `cleanup_task_branches_post_close` call lives in
/// the coordinator's `task_merge.rs` (gated by the same helpers) and
/// is covered there in `task_merge::tests`.
#[tokio::test]
async fn mirror_carries_task_branch_and_checkpoint_ref_independently() {
    let source_dir = TempDir::new().unwrap();
    seed_source_repo(source_dir.path()).await;
    let mirrors_dir = TempDir::new().unwrap();
    let (mgr, project_id, _tip) = build_mirror_with_task_branch_and_checkpoint(
        mirrors_dir.path(),
        source_dir.path(),
        "merge-safety",
    )
    .await;

    let task_branch = "refs/heads/task/merge-safety";
    let checkpoint_ref = format!("{CHECKPOINT_REF_PREFIX}task-merge-safety/session-1");

    // Both refs exist in the mirror.
    assert!(
        !git_stdout(
            &["git", "rev-parse", "--verify", task_branch],
            &mgr.mirror_path(&project_id)
        )
        .await
        .is_empty(),
        "task branch ref must exist in mirror"
    );
    assert!(
        !git_stdout(
            &["git", "rev-parse", "--verify", &checkpoint_ref],
            &mgr.mirror_path(&project_id)
        )
        .await
        .is_empty(),
        "checkpoint ref must exist in mirror"
    );

    // `git for-each-ref` (the same enumeration the post-close inventory
    // uses) must surface both refs independently.
    let inventory = git_stdout(
        &[
            "git",
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads/task/",
            CHECKPOINT_REF_PREFIX,
        ],
        &mgr.mirror_path(&project_id),
    )
    .await;
    assert!(
        inventory.contains(task_branch),
        "inventory must list the task branch ref"
    );
    assert!(
        inventory.contains(&checkpoint_ref),
        "inventory must list the checkpoint ref"
    );

    // The classification helpers must distinguish them so the cleanup
    // path knows which to delete and which to leave alone.
    assert_eq!(classify_ref(task_branch), RefRole::TaskBranch);
    assert_eq!(classify_ref(&checkpoint_ref), RefRole::CheckpointRef);
    assert!(!is_checkpoint_ref(task_branch));
    assert!(is_checkpoint_ref(&checkpoint_ref));
    assert!(!is_protected_ref(task_branch));
}

// ─── protected-ref guard ─────────────────────────────────────────────────

/// Acceptance criterion 3 (sy0g): protected branches (integration
/// targets) are never eligible for cleanup, even though a `main`
/// branch may share the same shape as a task branch minus the `task/`
/// prefix. The classify path must distinguish them.
#[tokio::test]
async fn main_branch_is_protected_and_not_safe_to_cleanup() {
    // Build a bare source repo whose `main` branch exists; classify it
    // and assert the protected/safe-to-cleanup transitions are correct.
    let source_dir = TempDir::new().unwrap();
    seed_source_repo(source_dir.path()).await;

    let mirrors_dir = TempDir::new().unwrap();
    let mgr = MirrorManager::new(mirrors_dir.path().to_path_buf());
    let project_id = "proj-main-guard".to_string();
    let source_url = format!("file://{}", source_dir.path().display());
    mgr.ensure_mirror(&project_id, &source_url).await.unwrap();

    // The mirror has `refs/heads/main` after the ensure_mirror fetch.
    assert!(
        !git_stdout(
            &["git", "rev-parse", "--verify", "refs/heads/main"],
            &mgr.mirror_path(&project_id)
        )
        .await
        .is_empty(),
        "fixture precondition: mirror must carry refs/heads/main"
    );

    // Classification must treat `main` as Protected.
    assert_eq!(classify_ref("main"), RefRole::Protected);
    assert_eq!(classify_ref("refs/heads/main"), RefRole::Protected);
    assert!(is_protected_ref("main"));
    assert!(is_protected_ref("refs/heads/main"));

    // Protected refs must NOT be marked safe-to-cleanup so the
    // automated branch sweep refuses to delete them.
    assert!(!RefRole::Protected.is_safe_to_cleanup());

    // Protected refs must NOT be eligible as a final-merge source —
    // merges go INTO `main`, never OUT of it. (If `main` were treated
    // as eligible, the merge path could attempt to merge main into
    // itself, which is meaningless and corrupting.)
    assert!(!RefRole::Protected.is_eligible_final_merge_source());
}

// ─── squash merge safety ─────────────────────────────────────────────────

/// Acceptance criterion 2 (sy0g): final merge behavior remains
/// squash-based. The merge helpers in `djinn-workspace::Workspace`
/// already use `--squash` (see `try_merge` / the `detect_pr_conflict_files`
/// shape used by the PR poller). We pin the helper set here so a
/// future refactor that changes `Workspace::try_merge` to a non-squash
/// flow trips this test and forces a deliberate decision.
///
/// Concretely: the `Workspace` API exposes `try_merge` (which runs
/// `git merge --no-commit --no-ff` — not squash, because the worker
/// still has to resolve conflicts and produce a clean tree), and
/// `detect_pr_conflict_files` (which uses `merge --squash`). The
/// squash semantics the PR poller relies on live in
/// `detect_pr_conflict_files`. This test pins the public surface so
/// a future regression that swaps `--no-ff` for `--squash` (or vice
/// versa) on `try_merge` doesn't accidentally change the merge head
/// a future contributor picks up.
#[tokio::test]
async fn workspace_try_merge_uses_no_ff_not_squash() {
    // Read the `try_merge` git-invocation source to confirm it still uses
    // `--no-commit --no-ff` (the worker still produces a merge commit, which
    // the supervisor's `enforce_merge_parent` then converts to a true
    // two-parent merge). The squash landing path is `detect_pr_conflict_files`
    // + GitHub's `merge_pull_request(MergeMethod::Squash, ...)`.
    //
    // The actual `git merge` shell-out was migrated out of `workspace.rs` into
    // the `git_helpers` module (routed through `djinn-git`), so pin the
    // invocation there. `Workspace::try_merge` delegates to
    // `git_helpers::try_merge_no_commit_no_ff`.
    let source = include_str!("../src/git_helpers.rs");
    assert!(
        source.contains("\"merge\".to_string()")
            && source.contains("\"--no-commit\".to_string()")
            && source.contains("\"--no-ff\".to_string()"),
        "git_helpers::try_merge_no_commit_no_ff must remain `--no-commit --no-ff` so the worker \
         produces a merge commit; the squash landing semantics live in `detect_pr_conflict_files` \
         + the PR poller's `merge_pull_request(MergeMethod::Squash, ...)` call"
    );
    assert!(
        !source.contains("\"--squash\""),
        "git_helpers::try_merge_no_commit_no_ff must not switch to `--squash`; squash landing \
         lives in `detect_pr_conflict_files` + the PR poller, not the worker's mid-run merge"
    );
}
