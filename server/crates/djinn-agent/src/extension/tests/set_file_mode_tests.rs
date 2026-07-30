//! Dispatch-level contract for `set_file_mode`.
//!
//! The handler exists because `chmod` from the agent's shell cannot work:
//! inode-metadata operations are owner-only, and files written through
//! `write`/`edit` belong to the worker process (uid 1000) while the shell runs
//! as the launcher-spawned child (uid 1001). These tests run in-process, so they
//! exercise the owner path — the same path the worker takes in production.
//!
//! What they cannot exercise is the cross-uid refusal itself: a test process owns
//! everything it creates. That boundary is asserted where it is enforced (the
//! launcher's `CHILD_UID` contract); here the coverage is the *narrowing* around
//! the operation — regular files only, no special bits, no escape from the
//! worktree.

use super::*;
use std::os::unix::fs::PermissionsExt;

fn args(value: serde_json::Value) -> Option<serde_json::Map<String, serde_json::Value>> {
    Some(value.as_object().expect("object").clone())
}

fn mode_of(path: &std::path::Path) -> u32 {
    std::fs::metadata(path).expect("stat").permissions().mode() & 0o7777
}

#[tokio::test]
async fn sets_and_clears_the_executable_bit() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-setmode-");
    let file = worktree.path().join("gate.sh");
    tokio::fs::write(&file, "#!/usr/bin/env bash\necho hi\n")
        .await
        .expect("seed");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).expect("seed mode");

    let response = call_set_file_mode(
        &args(serde_json::json!({"path": "gate.sh", "executable": true})),
        worktree.path(),
    )
    .await
    .expect("set +x");

    assert_eq!(mode_of(&file), 0o755, "disk must actually be executable");
    assert_eq!(response["executable"], true);
    assert_eq!(response["mode"], "0755");
    assert_eq!(response["previous_mode"], "0644");
    // The response points the agent at the tree, not the index — the whole
    // lesson of tv9g, where the index read green four times.
    assert_eq!(response["expected_git_mode"], "100755");

    let response = call_set_file_mode(
        &args(serde_json::json!({"path": "gate.sh", "executable": false})),
        worktree.path(),
    )
    .await
    .expect("set -x");

    assert_eq!(mode_of(&file), 0o644);
    assert_eq!(response["expected_git_mode"], "100644");
}

/// Content must be untouched — this tool changes a mode, not bytes. If it ever
/// rewrote the file it would also have to invalidate the read record, and the
/// handler deliberately does not.
#[tokio::test]
async fn leaves_file_content_untouched() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-setmode-content-");
    let file = worktree.path().join("script.sh");
    let original = "#!/bin/sh\necho unchanged\n";
    tokio::fs::write(&file, original).await.expect("seed");

    call_set_file_mode(
        &args(serde_json::json!({"path": "script.sh", "executable": true})),
        worktree.path(),
    )
    .await
    .expect("set +x");

    assert_eq!(
        tokio::fs::read_to_string(&file).await.expect("read"),
        original
    );
}

#[tokio::test]
async fn is_idempotent() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-setmode-idem-");
    let file = worktree.path().join("a.sh");
    tokio::fs::write(&file, "x\n").await.expect("seed");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).expect("seed mode");

    let response = call_set_file_mode(
        &args(serde_json::json!({"path": "a.sh", "executable": true})),
        worktree.path(),
    )
    .await
    .expect("set +x");

    assert_eq!(response["unchanged"], true, "already executable is a no-op");
    assert_eq!(mode_of(&file), 0o755);
}

/// setuid/setgid/sticky are refused rather than preserved (which would ship an
/// agent-triggered setuid binary) or cleared (an unrequested mode change).
#[tokio::test]
async fn refuses_a_file_carrying_special_bits() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-setmode-setuid-");

    for (label, mode) in [("setuid", 0o4644), ("setgid", 0o2644), ("sticky", 0o1644)] {
        let name = format!("{label}.sh");
        let file = worktree.path().join(&name);
        tokio::fs::write(&file, "x\n").await.expect("seed");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).expect("seed mode");
        // Some filesystems refuse setgid/sticky for an unprivileged user; skip
        // rather than assert a kernel behaviour this test does not own.
        if mode_of(&file) & 0o7000 == 0 {
            continue;
        }

        let error = call_set_file_mode(
            &args(serde_json::json!({"path": name, "executable": true})),
            worktree.path(),
        )
        .await
        .expect_err("must refuse a file with special bits");
        assert!(
            error.contains("setuid/setgid/sticky"),
            "{label}: unexpected error: {error}"
        );

        assert_eq!(
            mode_of(&file) & 0o7000,
            mode & 0o7000,
            "{label}: a refusal must not modify the file"
        );
    }
}

#[tokio::test]
async fn refuses_a_directory() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-setmode-dir-");
    tokio::fs::create_dir(worktree.path().join("somedir"))
        .await
        .expect("mkdir");

    let error = call_set_file_mode(
        &args(serde_json::json!({"path": "somedir", "executable": true})),
        worktree.path(),
    )
    .await
    .expect_err("must refuse a directory");
    assert!(error.contains("regular files"), "unexpected error: {error}");
}

#[tokio::test]
async fn refuses_a_path_outside_the_worktree() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-setmode-escape-");
    let outside = crate::test_helpers::test_tempdir("djinn-ext-setmode-outside-");
    let victim = outside.path().join("victim.sh");
    tokio::fs::write(&victim, "x\n").await.expect("seed");
    std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).expect("seed mode");

    for path in [
        victim.display().to_string(),
        format!(
            "../{}/victim.sh",
            outside.path().file_name().unwrap().to_string_lossy()
        ),
    ] {
        let error = call_set_file_mode(
            &args(serde_json::json!({"path": path, "executable": true})),
            worktree.path(),
        )
        .await
        .expect_err("must refuse a path outside the worktree");
        assert!(
            error.contains("outside worktree"),
            "path {path}: unexpected error: {error}"
        );
    }

    assert_eq!(
        mode_of(&victim),
        0o644,
        "a refused path must not have been modified"
    );
}

/// A symlink is resolved BEFORE the containment check, so one pointing out of
/// the worktree is rejected rather than followed. `set_permissions` follows
/// symlinks, so without this the escape would land on the target.
#[tokio::test]
async fn refuses_a_symlink_escaping_the_worktree() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-setmode-symlink-");
    let outside = crate::test_helpers::test_tempdir("djinn-ext-setmode-symlink-out-");
    let victim = outside.path().join("victim.sh");
    tokio::fs::write(&victim, "x\n").await.expect("seed");
    std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).expect("seed mode");

    std::os::unix::fs::symlink(&victim, worktree.path().join("link.sh")).expect("symlink");

    let error = call_set_file_mode(
        &args(serde_json::json!({"path": "link.sh", "executable": true})),
        worktree.path(),
    )
    .await
    .expect_err("must refuse a symlink pointing outside the worktree");
    assert!(
        error.contains("outside worktree"),
        "unexpected error: {error}"
    );

    assert_eq!(
        mode_of(&victim),
        0o644,
        "the symlink target outside the worktree must be untouched"
    );
}

#[tokio::test]
async fn reports_a_missing_file_rather_than_creating_one() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-setmode-missing-");

    let error = call_set_file_mode(
        &args(serde_json::json!({"path": "nope.sh", "executable": true})),
        worktree.path(),
    )
    .await
    .expect_err("must not invent a file");
    assert!(error.contains("cannot stat"), "unexpected error: {error}");
    assert!(!worktree.path().join("nope.sh").exists());
}

#[tokio::test]
async fn rejects_a_missing_executable_argument() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-setmode-args-");
    tokio::fs::write(worktree.path().join("a.sh"), "x\n")
        .await
        .expect("seed");

    call_set_file_mode(&args(serde_json::json!({"path": "a.sh"})), worktree.path())
        .await
        .expect_err("`executable` is required; defaulting it would guess a direction");
}
