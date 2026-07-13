// Shell GateGuard dispatch-level tests.
//
// These tests exercise the full dispatch path through `call_shell` with
// session_role parameters, proving that the worker shell GateGuard
// enforcement is applied at the handler level.  The colocated
// `command_classifier_tests` module covers the classifier itself; these
// cover the dispatch integration via `gate_guard_shell_check`.

use super::handlers::call_shell;
use super::{agent_context_from_db, create_test_db};
use crate::file_time::destructive_class::WORKTREE_LOCAL_FILE_MUTATION;
use tokio_util::sync::CancellationToken;

fn setup(prefix: &str) -> (tempfile::TempDir, crate::context::AgentContext) {
    let dir = crate::test_helpers::test_tempdir(prefix);
    let db = create_test_db();
    let state = agent_context_from_db(db, CancellationToken::new());
    (dir, state)
}

fn shell_args(command: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    Some(
        serde_json::json!({ "command": command })
            .as_object()
            .unwrap()
            .clone(),
    )
}

// ─── AC 1: worker soft-gate first-deny / second-allow ────────────────────

/// Worker soft-gated command (`rm scratch.txt`) returns a FORCE prompt on
/// first invocation and records `bash_soft_forced` for the
/// `WorktreeLocalFileMutation` class.  After re-creating the scratch file,
/// retry with the same command succeeds (soft-gate allows second call).
#[tokio::test]
async fn shell_worker_soft_gate_first_deny_second_allow() {
    let (worktree, state) = setup("gg-shell-softgate-");
    let session_id = worktree.path().display().to_string();

    // Create a scratch file that the soft-gated `rm` will target.
    let scratch = worktree.path().join("scratch.txt");
    tokio::fs::write(&scratch, "temporary\n")
        .await
        .expect("seed scratch");

    // Phase 1: first worker call → soft-gated, returns FORCE prompt.
    let err = call_shell(
        &state,
        &shell_args("rm scratch.txt"),
        worktree.path(),
        Some("worker"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect_err("first soft-gated shell call must be denied");
    assert!(
        err.contains("gated"),
        "expected FORCE prompt with 'gated', got: {err}"
    );
    assert!(
        err.contains("rollback") || err.contains("recreation"),
        "FORCE prompt must demand a rollback/recreation plan, got: {err}"
    );

    // bash_soft_forced must be set for WorktreeLocalFileMutation.
    assert!(
        state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        "bash_soft_forced must be set for WorktreeLocalFileMutation after first denial"
    );

    // Phase 2: re-create the scratch file (rm hasn't run yet), retry same
    // command → must succeed and actually remove the file.
    tokio::fs::write(&scratch, "temporary\n")
        .await
        .expect("re-seed scratch");
    let resp = call_shell(
        &state,
        &shell_args("rm scratch.txt"),
        worktree.path(),
        Some("worker"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect("second soft-gated shell call must succeed");
    assert_eq!(resp["ok"], serde_json::json!(true), "rm must exit cleanly");

    // The file must actually be removed.
    assert!(
        !scratch.exists(),
        "scratch.txt must be removed after successful retry"
    );

    // bash_soft_forced must still be set (no reset on success).
    assert!(
        state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        "bash_soft_forced must remain set after successful retry"
    );
}

// ─── AC 2: hard-deny commands ────────────────────────────────────────────

/// Hard-denied shell command (`git reset --hard`) returns an error, never
/// marks `bash_soft_forced`, and cannot be unlocked by retry.
#[tokio::test]
async fn shell_hard_deny_git_reset_hard_never_marks_bash_soft_forced() {
    let (worktree, state) = setup("gg-shell-harddeny-git-");
    let session_id = worktree.path().display().to_string();

    let err = call_shell(
        &state,
        &shell_args("git reset --hard HEAD"),
        worktree.path(),
        Some("worker"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect_err("git reset --hard must be hard-denied");
    assert!(
        err.contains("forbidden"),
        "hard-deny must contain 'forbidden', got: {err}"
    );
    assert!(
        err.contains("cannot be unlocked") || err.contains("retry"),
        "hard-deny must say no-unlock, got: {err}"
    );

    // bash_soft_forced must NOT be set for any class.
    assert!(
        !state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        "hard-denied command must NOT mark bash_soft_forced for WorktreeLocalFileMutation"
    );
}

/// Hard-denied shell command (`git clean -fd`) returns an error and never
/// marks `bash_soft_forced`.
#[tokio::test]
async fn shell_hard_deny_git_clean_never_marks_bash_soft_forced() {
    let (worktree, state) = setup("gg-shell-harddeny-clean-");
    let session_id = worktree.path().display().to_string();

    let err = call_shell(
        &state,
        &shell_args("git clean -fd"),
        worktree.path(),
        Some("worker"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect_err("git clean must be hard-denied");
    assert!(
        err.contains("forbidden"),
        "hard-deny must contain 'forbidden', got: {err}"
    );

    assert!(
        !state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        "git clean must NOT mark bash_soft_forced"
    );
}

/// Hard-denied shell command (`git stash`) returns an error and never marks
/// `bash_soft_forced`.
#[tokio::test]
async fn shell_hard_deny_git_stash_never_marks_bash_soft_forced() {
    let (worktree, state) = setup("gg-shell-harddeny-stash-");
    let session_id = worktree.path().display().to_string();

    let err = call_shell(
        &state,
        &shell_args("git stash"),
        worktree.path(),
        Some("worker"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect_err("git stash must be hard-denied");
    assert!(
        err.contains("forbidden"),
        "hard-deny must contain 'forbidden', got: {err}"
    );

    assert!(
        !state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        "git stash must NOT mark bash_soft_forced"
    );
}

/// Hard-denied DB DDL command (`psql -c "DROP TABLE foo"`) returns an error
/// and never marks `bash_soft_forced`.
#[tokio::test]
async fn shell_hard_deny_db_drop_table_never_marks_bash_soft_forced() {
    let (worktree, state) = setup("gg-shell-harddeny-db-ddl-");
    let session_id = worktree.path().display().to_string();

    let err = call_shell(
        &state,
        &shell_args("psql -c \"DROP TABLE foo\""),
        worktree.path(),
        Some("worker"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect_err("DROP TABLE must be hard-denied");
    assert!(
        err.contains("forbidden"),
        "hard-deny must contain 'forbidden', got: {err}"
    );

    assert!(
        !state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        "DROP TABLE must NOT mark bash_soft_forced"
    );
}

/// Hard-denied DB DML command (`psql -c "DELETE FROM users"`) returns an
/// error and never marks `bash_soft_forced`.
#[tokio::test]
async fn shell_hard_deny_db_delete_from_never_marks_bash_soft_forced() {
    let (worktree, state) = setup("gg-shell-harddeny-db-dml-");
    let session_id = worktree.path().display().to_string();

    let err = call_shell(
        &state,
        &shell_args("psql -c \"DELETE FROM users WHERE id = 1\""),
        worktree.path(),
        Some("worker"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect_err("DELETE FROM must be hard-denied");
    assert!(
        err.contains("forbidden"),
        "hard-deny must contain 'forbidden', got: {err}"
    );

    assert!(
        !state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        "DELETE FROM must NOT mark bash_soft_forced"
    );
}

// ─── AC 3: non-worker role bypass ────────────────────────────────────────

/// Reviewer runs a destructive-class command (`rm tmp.txt`) without gate
/// guard interference.  The command executes successfully and
/// `bash_soft_forced` is never recorded.
#[tokio::test]
async fn shell_reviewer_bypasses_gate_guard_no_bash_soft_forced() {
    let (worktree, state) = setup("gg-shell-reviewer-");
    let session_id = worktree.path().display().to_string();

    // Create a file for the reviewer's `rm` to target.
    let tmp = worktree.path().join("tmp.txt");
    tokio::fs::write(&tmp, "data\n").await.expect("seed");

    let resp = call_shell(
        &state,
        &shell_args("rm tmp.txt"),
        worktree.path(),
        Some("reviewer"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect("reviewer shell call must succeed (no GateGuard)");
    assert_eq!(resp["ok"], serde_json::json!(true));
    assert!(!tmp.exists(), "reviewer rm must actually remove the file");

    // bash_soft_forced must NOT be recorded for non-worker roles.
    assert!(
        !state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        "reviewer must NOT mark bash_soft_forced"
    );
}

/// Planner runs a destructive-class command without gate guard
/// interference.  `bash_soft_forced` is never recorded.
#[tokio::test]
async fn shell_planner_bypasses_gate_guard_no_bash_soft_forced() {
    let (worktree, state) = setup("gg-shell-planner-");
    let session_id = worktree.path().display().to_string();

    let tmp = worktree.path().join("planner-tmp.txt");
    tokio::fs::write(&tmp, "data\n").await.expect("seed");

    let resp = call_shell(
        &state,
        &shell_args("rm planner-tmp.txt"),
        worktree.path(),
        Some("planner"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect("planner shell call must succeed");
    assert_eq!(resp["ok"], serde_json::json!(true));

    assert!(
        !state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        "planner must NOT mark bash_soft_forced"
    );
}

/// Architect runs a destructive-class command without gate guard
/// interference.  `bash_soft_forced` is never recorded.
#[tokio::test]
async fn shell_architect_bypasses_gate_guard_no_bash_soft_forced() {
    let (worktree, state) = setup("gg-shell-architect-");
    let session_id = worktree.path().display().to_string();

    let tmp = worktree.path().join("arch-tmp.txt");
    tokio::fs::write(&tmp, "data\n").await.expect("seed");

    let resp = call_shell(
        &state,
        &shell_args("rm arch-tmp.txt"),
        worktree.path(),
        Some("architect"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect("architect shell call must succeed");
    assert_eq!(resp["ok"], serde_json::json!(true));

    assert!(
        !state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        "architect must NOT mark bash_soft_forced"
    );
}

/// Missing role (None) runs a destructive-class command without gate guard
/// interference.  `bash_soft_forced` is never recorded.
#[tokio::test]
async fn shell_missing_role_bypasses_gate_guard_no_bash_soft_forced() {
    let (worktree, state) = setup("gg-shell-norole-");
    let session_id = worktree.path().display().to_string();

    let tmp = worktree.path().join("no-role-tmp.txt");
    tokio::fs::write(&tmp, "data\n").await.expect("seed");

    let resp = call_shell(
        &state,
        &shell_args("rm no-role-tmp.txt"),
        worktree.path(),
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect("missing-role shell call must succeed");
    assert_eq!(resp["ok"], serde_json::json!(true));

    assert!(
        !state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        "missing role must NOT mark bash_soft_forced"
    );
}

// ─── AC 4: path-scope exclusions through the enforcement path ────────────

/// Commands targeting `.git` paths are hard-denied through the full dispatch
/// path (not just the classifier unit test).  `bash_soft_forced` is never
/// recorded.
#[tokio::test]
async fn shell_path_scope_git_is_hard_denied() {
    let (worktree, state) = setup("gg-shell-path-git-");
    let session_id = worktree.path().display().to_string();

    let err = call_shell(
        &state,
        &shell_args("rm .git/objects/pack"),
        worktree.path(),
        Some("worker"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect_err("rm targeting .git must be hard-denied");
    assert!(
        err.contains("forbidden"),
        "hard-deny must contain 'forbidden', got: {err}"
    );

    assert!(
        !state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        ".git path must NOT mark bash_soft_forced"
    );
}

/// Commands targeting parent directory (`..`) paths are hard-denied through
/// the full dispatch path.  `bash_soft_forced` is never recorded.
#[tokio::test]
async fn shell_path_scope_parent_dir_is_hard_denied() {
    let (worktree, state) = setup("gg-shell-path-dotdot-");
    let session_id = worktree.path().display().to_string();

    let err = call_shell(
        &state,
        &shell_args("rm ../outside-file"),
        worktree.path(),
        Some("worker"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect_err("rm targeting .. must be hard-denied");
    assert!(
        err.contains("forbidden"),
        "hard-deny must contain 'forbidden', got: {err}"
    );

    assert!(
        !state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        ".. path must NOT mark bash_soft_forced"
    );
}

/// Commands targeting `.djinn/read-sources` paths are hard-denied through
/// the full dispatch path.  `bash_soft_forced` is never recorded.
#[tokio::test]
async fn shell_path_scope_djinn_read_sources_is_hard_denied() {
    let (worktree, state) = setup("gg-shell-path-djinnrs-");
    let session_id = worktree.path().display().to_string();

    let err = call_shell(
        &state,
        &shell_args("rm .djinn/read-sources/some-project/file.txt"),
        worktree.path(),
        Some("worker"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect_err("rm targeting .djinn/read-sources must be hard-denied");
    assert!(
        err.contains("forbidden"),
        "hard-deny must contain 'forbidden', got: {err}"
    );

    assert!(
        !state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        ".djinn/read-sources path must NOT mark bash_soft_forced"
    );
}

/// Commands targeting durable data paths (`Cargo.toml`) are hard-denied
/// through the full dispatch path.  `bash_soft_forced` is never recorded.
#[tokio::test]
async fn shell_path_scope_durable_data_cargo_toml_is_hard_denied() {
    let (worktree, state) = setup("gg-shell-path-durable-");
    let session_id = worktree.path().display().to_string();

    let err = call_shell(
        &state,
        &shell_args("rm Cargo.toml"),
        worktree.path(),
        Some("worker"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect_err("rm targeting Cargo.toml must be hard-denied");
    assert!(
        err.contains("forbidden"),
        "hard-deny must contain 'forbidden', got: {err}"
    );

    assert!(
        !state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        "Cargo.toml durable data path must NOT mark bash_soft_forced"
    );
}

/// Commands targeting durable data paths (`package.json`) are hard-denied
/// through the full dispatch path.
#[tokio::test]
async fn shell_path_scope_durable_data_package_json_is_hard_denied() {
    let (worktree, state) = setup("gg-shell-path-durable-npm-");
    let session_id = worktree.path().display().to_string();

    let err = call_shell(
        &state,
        &shell_args("rm package.json"),
        worktree.path(),
        Some("worker"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect_err("rm targeting package.json must be hard-denied");
    assert!(
        err.contains("forbidden"),
        "hard-deny must contain 'forbidden', got: {err}"
    );

    assert!(
        !state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        "package.json durable data path must NOT mark bash_soft_forced"
    );
}

/// Commands targeting durable data paths (`.gitignore`) are hard-denied
/// through the full dispatch path.
#[tokio::test]
async fn shell_path_scope_durable_data_gitignore_is_hard_denied() {
    let (worktree, state) = setup("gg-shell-path-durable-gi-");
    let session_id = worktree.path().display().to_string();

    let err = call_shell(
        &state,
        &shell_args("rm .gitignore"),
        worktree.path(),
        Some("worker"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect_err("rm targeting .gitignore must be hard-denied");
    assert!(
        err.contains("forbidden"),
        "hard-deny must contain 'forbidden', got: {err}"
    );

    assert!(
        !state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        ".gitignore durable data path must NOT mark bash_soft_forced"
    );
}
