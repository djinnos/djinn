use super::*;
use std::os::unix::fs::{FileTypeExt, MetadataExt};

// ── Byte-for-byte preservation helpers ──────────────────────────────────

/// Snapshot every entry in a directory tree, including Git's index and object
/// data, for byte-for-byte comparison before/after a migration attempt.
///
/// This deliberately uses no-follow metadata and never reads a special file:
/// opening a FIFO would block and following a symlink would snapshot the wrong
/// object. For special files the no-follow type, mode, and device identity are
/// part of the snapshot instead of file contents.
pub(super) fn snapshot_bytes(path: &Path) -> Vec<u8> {
    let mut buf = Vec::new();
    snapshot_bytes_into(path, &mut buf);
    buf
}

fn snapshot_bytes_into(path: &Path, buf: &mut Vec<u8>) {
    let metadata = fs::symlink_metadata(path)
        .unwrap_or_else(|error| panic!("symlink_metadata {path:?}: {error}"));
    snapshot_field(buf, path.as_os_str().as_encoded_bytes());
    if metadata.file_type().is_symlink() {
        buf.push(b'l');
        let target =
            fs::read_link(path).unwrap_or_else(|error| panic!("read_link {path:?}: {error}"));
        snapshot_field(buf, target.as_os_str().as_encoded_bytes());
        return;
    }
    if metadata.is_file() {
        buf.push(b'f');
        let bytes = fs::read(path).unwrap_or_else(|error| panic!("read {path:?}: {error}"));
        snapshot_field(buf, &bytes);
        return;
    }
    if metadata.is_dir() {
        buf.push(b'd');
        let mut entries: Vec<_> = fs::read_dir(path)
            .unwrap_or_else(|e| panic!("read_dir {path:?}: {e}"))
            .map(|entry| entry.unwrap_or_else(|error| panic!("read_dir entry {path:?}: {error}")))
            .collect();
        entries.sort_by_key(|e| e.path());
        for entry in entries {
            snapshot_bytes_into(&entry.path(), buf);
        }
        return;
    }

    // A special file has no safely-readable byte stream. Preserve its exact
    // no-follow filesystem identity instead; this distinguishes FIFOs from
    // devices and sockets and catches a replacement with any other object.
    buf.push(b's');
    snapshot_field(buf, special_file_kind(&metadata).as_bytes());
    buf.extend_from_slice(&metadata.mode().to_le_bytes());
    buf.extend_from_slice(&metadata.rdev().to_le_bytes());
}

fn snapshot_field(buf: &mut Vec<u8>, bytes: &[u8]) {
    buf.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    buf.extend_from_slice(bytes);
}

fn special_file_kind(metadata: &fs::Metadata) -> &'static str {
    let file_type = metadata.file_type();
    if file_type.is_fifo() {
        "fifo"
    } else if file_type.is_socket() {
        "socket"
    } else if file_type.is_block_device() {
        "block_device"
    } else if file_type.is_char_device() {
        "char_device"
    } else {
        "unknown_special"
    }
}

// ── AC1: Table-driven repository classification matrix ─────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn classify_single_clean_input() {
    // AC1: one clean detached input → migration succeeds, input preserved.
    let fx = Fixture::new().await;
    let project_path = fx.project_legacy_path();
    make_clean_checkout(&fx.mirror_path, &project_path);

    let before = snapshot_bytes(&project_path);
    let migrator = fx.migrator();
    let result = migrator
        .migrate(fx.request(vec![fx.legacy_input(LegacyKind::ProjectLocal)]))
        .await
        .unwrap();
    assert!(matches!(result, ReadSourceMigrationResult::Published(_)));
    let after = snapshot_bytes(&project_path);
    assert_eq!(before, after, "clean legacy input preserved byte-for-byte");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn classify_staged_tracked_content() {
    // AC1: staged tracked dirt → fails closed, typed classification preserved.
    let fx = Fixture::new().await;
    let project_path = fx.project_legacy_path();
    make_clean_checkout(&fx.mirror_path, &project_path);
    // Stage a modification.
    fs::write(project_path.join("README.md"), "staged\n").unwrap();
    run_git(&project_path, &["add", "README.md"]);

    let before = snapshot_bytes(&project_path);
    let migrator = fx.migrator();
    let result = migrator
        .migrate(fx.request(vec![fx.legacy_input(LegacyKind::ProjectLocal)]))
        .await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(&err, ReadSourceMigrationError::Ambiguous(d) if d.contains("dirty_tracked")),
        "expected dirty_tracked, got: {err}"
    );
    assert_eq!(
        classify(&project_path, &fx.target_commit),
        ReadSourcePathState::DirtyTracked
    );
    let after = snapshot_bytes(&project_path);
    assert_eq!(before, after, "staged dirty legacy preserved byte-for-byte");
    assert!(!fx.destination().exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn classify_special_file_input_preserves_byte_for_byte() {
    // AC1: special file input (fifo) → fails closed, typed classification.
    let fx = Fixture::new().await;
    let project_path = fx.project_legacy_path();
    fs::create_dir_all(project_path.parent().unwrap()).unwrap();
    let status = std::process::Command::new("mkfifo")
        .arg(&project_path)
        .status()
        .expect("mkfifo available on unix");
    assert!(status.success());
    let before = snapshot_bytes(&project_path);

    let migrator = fx.migrator();
    let result = migrator
        .migrate(fx.request(vec![fx.legacy_input(LegacyKind::ProjectLocal)]))
        .await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(&err, ReadSourceMigrationError::Ambiguous(d) if d.contains("special")),
        "expected special classification, got: {err}"
    );
    assert_eq!(
        snapshot_bytes(&project_path),
        before,
        "special legacy preserves its exact no-follow identity"
    );
    assert!(!fx.destination().exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn classify_invalid_git_input_preserves_byte_for_byte() {
    // AC1/AC3: InvalidGit (malformed/partial git) legacy input → fails closed.
    let fx = Fixture::new().await;
    let project_path = fx.project_legacy_path();
    // Create a directory with a .git that is not a valid git repo.
    fs::create_dir_all(&project_path).unwrap();
    fs::create_dir_all(project_path.join(".git")).unwrap();
    fs::write(project_path.join("README.md"), "not a real repo\n").unwrap();

    let before = snapshot_bytes(&project_path);
    let migrator = fx.migrator();
    let result = migrator
        .migrate(fx.request(vec![fx.legacy_input(LegacyKind::ProjectLocal)]))
        .await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(&err, ReadSourceMigrationError::Ambiguous(d) if d.contains("invalid_git")),
        "expected invalid_git classification, got: {err}"
    );
    // Legacy preserved byte-for-byte.
    let after = snapshot_bytes(&project_path);
    assert_eq!(before, after, "invalid_git legacy preserved byte-for-byte");
    assert!(!fx.destination().exists());
}

// ── AC2: Destination classification ─────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn destination_missing_is_published() {
    // AC2: missing destination → published.
    let fx = Fixture::new().await;
    let migrator = fx.migrator();
    let result = migrator.migrate(fx.request(vec![])).await.unwrap();
    assert!(matches!(result, ReadSourceMigrationResult::Published(_)));
    assert_eq!(
        classify(&fx.destination(), &fx.target_commit),
        ReadSourcePathState::Clean {
            commit: fx.target_commit.clone()
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn destination_valid_idempotent_for_same_owner_target() {
    // AC2: a valid clean destination is accepted idempotently for the same
    // (owner, target). Two migrations produce the same result.
    let fx = Fixture::new().await;
    let dest = fx.destination();
    make_clean_checkout(&fx.mirror_path, &dest);
    let before = snapshot_bytes(&dest);

    let migrator = fx.migrator();
    let r1 = migrator.migrate(fx.request(vec![])).await.unwrap();
    assert!(matches!(r1, ReadSourceMigrationResult::Existing(_)));

    let r2 = migrator.migrate(fx.request(vec![])).await.unwrap();
    assert!(matches!(r2, ReadSourceMigrationResult::Existing(_)));

    let after = snapshot_bytes(&dest);
    assert_eq!(
        before, after,
        "destination unchanged across idempotent re-migration"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn destination_dirty_fails_closed_preserved() {
    // AC2: dirty destination → fails closed, preserved.
    let fx = Fixture::new().await;
    let dest = fx.destination();
    make_clean_checkout(&fx.mirror_path, &dest);
    fs::write(dest.join("README.md"), "dirty\n").unwrap();
    let before = snapshot_bytes(&dest);

    let migrator = fx.migrator();
    let result = migrator.migrate(fx.request(vec![])).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(&err, ReadSourceMigrationError::Ambiguous(d) if d.contains("dirty_tracked")),
        "expected dirty_tracked, got: {err}"
    );
    let after = snapshot_bytes(&dest);
    assert_eq!(before, after, "dirty destination preserved byte-for-byte");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn destination_invalid_git_fails_closed() {
    // AC2: malformed/partial InvalidGit destination → fails closed.
    let fx = Fixture::new().await;
    let dest = fx.destination();
    // Create a directory with a partial .git (no objects).
    fs::create_dir_all(&dest).unwrap();
    fs::create_dir_all(dest.join(".git")).unwrap();
    fs::write(dest.join("README.md"), "partial\n").unwrap();

    let before = snapshot_bytes(&dest);
    let migrator = fx.migrator();
    let result = migrator.migrate(fx.request(vec![])).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(&err, ReadSourceMigrationError::Ambiguous(d) if d.contains("invalid_git")),
        "expected invalid_git, got: {err}"
    );
    let after = snapshot_bytes(&dest);
    assert_eq!(
        before, after,
        "invalid_git destination preserved byte-for-byte"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn destination_identity_mismatch_with_mirror() {
    // AC2: a valid clean detached destination at a different commit than
    // the mirror is an identity mismatch → fails closed.
    let fx = Fixture::new().await;
    let dest = fx.destination();
    // Create a different-commit mirror.
    let source2 = fx.tmp_path.join("src-mismatch");
    fs::create_dir_all(&source2).unwrap();
    run_git(&source2, &["init"]);
    run_git(&source2, &["config", "user.email", "t@t.com"]);
    run_git(&source2, &["config", "user.name", "T"]);
    fs::write(source2.join("README.md"), "other\n").unwrap();
    run_git(&source2, &["add", "."]);
    run_git(&source2, &["commit", "-m", "other"]);
    let mirror2 = fx.tmp_path.join("mirror-mismatch2.git");
    run_git(
        &fx.tmp_path,
        &[
            "clone",
            "--bare",
            source2.to_str().unwrap(),
            mirror2.to_str().unwrap(),
        ],
    );
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    make_clean_checkout(&mirror2, &dest);
    let before = snapshot_bytes(&dest);

    let migrator = fx.migrator();
    let result = migrator.migrate(fx.request(vec![])).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(&err, ReadSourceMigrationError::Ambiguous(d) if d.contains("identity_mismatch")),
        "expected identity_mismatch, got: {err}"
    );
    let after = snapshot_bytes(&dest);
    assert_eq!(before, after, "identity-mismatch destination preserved");
}

// ── AC3: Valid destination never accepted before all legacy classified ──

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_dest_not_accepted_before_untracked_legacy() {
    // AC3: a valid destination exists but a legacy input has untracked content.
    let fx = Fixture::new().await;
    let dest = fx.destination();
    make_clean_checkout(&fx.mirror_path, &dest);
    let dest_before = snapshot_bytes(&dest);

    let project_path = fx.project_legacy_path();
    make_clean_checkout(&fx.mirror_path, &project_path);
    fs::write(project_path.join("untracked.txt"), "stuff\n").unwrap();
    let legacy_before = snapshot_bytes(&project_path);

    let migrator = fx.migrator();
    let result = migrator
        .migrate(fx.request(vec![fx.legacy_input(LegacyKind::ProjectLocal)]))
        .await;
    assert!(result.is_err(), "must fail closed");

    assert_eq!(
        snapshot_bytes(&dest),
        dest_before,
        "valid destination preserved"
    );
    assert_eq!(
        snapshot_bytes(&project_path),
        legacy_before,
        "legacy preserved"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_dest_not_accepted_before_ignored_legacy() {
    // AC3: a valid destination exists but a legacy input has ignored content.
    let fx = Fixture::new().await;
    let dest = fx.destination();
    make_clean_checkout(&fx.mirror_path, &dest);
    let dest_before = snapshot_bytes(&dest);

    let project_path = fx.project_legacy_path();
    make_clean_checkout(&fx.mirror_path, &project_path);
    fs::write(project_path.join(".git/info/exclude"), "*.ignored\n").unwrap();
    fs::write(project_path.join("file.ignored"), "ignored\n").unwrap();
    let legacy_before = snapshot_bytes(&project_path);

    let migrator = fx.migrator();
    let result = migrator
        .migrate(fx.request(vec![fx.legacy_input(LegacyKind::ProjectLocal)]))
        .await;
    assert!(result.is_err());

    assert_eq!(
        snapshot_bytes(&dest),
        dest_before,
        "valid destination preserved"
    );
    assert_eq!(
        snapshot_bytes(&project_path),
        legacy_before,
        "legacy preserved"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_dest_not_accepted_before_differing_dual() {
    // AC3: a valid destination exists but dual legacy inputs differ.
    let fx = Fixture::new().await;
    let dest = fx.destination();
    make_clean_checkout(&fx.mirror_path, &dest);
    let dest_before = snapshot_bytes(&dest);

    let project_path = fx.project_legacy_path();
    make_clean_checkout(&fx.mirror_path, &project_path);

    let source2 = fx.tmp_path.join("src-dual");
    fs::create_dir_all(&source2).unwrap();
    run_git(&source2, &["init"]);
    run_git(&source2, &["config", "user.email", "t@t.com"]);
    run_git(&source2, &["config", "user.name", "T"]);
    fs::write(source2.join("README.md"), "different\n").unwrap();
    run_git(&source2, &["add", "."]);
    run_git(&source2, &["commit", "-m", "other"]);
    let mirror2 = fx.tmp_path.join("mirror-dual.git");
    run_git(
        &fx.tmp_path,
        &[
            "clone",
            "--bare",
            source2.to_str().unwrap(),
            mirror2.to_str().unwrap(),
        ],
    );
    let task_path = fx
        .owner_root
        .join("workspace/.djinn-read-sources")
        .join(&fx.target_project_id);
    fs::create_dir_all(task_path.parent().unwrap()).unwrap();
    make_clean_checkout(&mirror2, &task_path);

    let legacy_before = snapshot_bytes(&project_path);
    let task_before = snapshot_bytes(&task_path);

    let migrator = fx.migrator();
    let result = migrator
        .migrate(fx.request(vec![
            fx.legacy_input(LegacyKind::ProjectLocal),
            fx.legacy_input(LegacyKind::TaskLocal),
        ]))
        .await;
    assert!(result.is_err());

    assert_eq!(
        snapshot_bytes(&dest),
        dest_before,
        "valid destination preserved"
    );
    assert_eq!(
        snapshot_bytes(&project_path),
        legacy_before,
        "legacy preserved"
    );
    assert_eq!(
        snapshot_bytes(&task_path),
        task_before,
        "task-local preserved"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_dest_not_accepted_before_unknown_entry() {
    // AC3: a valid destination exists but an unknown parent sibling is present.
    let fx = Fixture::new().await;
    let dest = fx.destination();
    make_clean_checkout(&fx.mirror_path, &dest);
    let dest_before = snapshot_bytes(&dest);

    let legacy_parent = fx.owner_root.join(".djinn/read-sources");
    fs::create_dir_all(&legacy_parent).unwrap();
    fs::create_dir_all(legacy_parent.join("unexpected-target")).unwrap();
    let unknown_before = snapshot_bytes(&legacy_parent);

    let migrator = fx.migrator();
    let result = migrator
        .migrate(fx.request(vec![fx.legacy_input(LegacyKind::ProjectLocal)]))
        .await;
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        ReadSourceMigrationError::UnknownEntry { .. }
    ));

    assert_eq!(
        snapshot_bytes(&dest),
        dest_before,
        "valid destination preserved"
    );
    assert_eq!(
        snapshot_bytes(&legacy_parent),
        unknown_before,
        "unknown entry preserved"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_dest_not_accepted_before_symlink_legacy() {
    // AC3: a valid destination exists but a legacy input is a symlink.
    let fx = Fixture::new().await;
    let dest = fx.destination();
    make_clean_checkout(&fx.mirror_path, &dest);
    let dest_before = snapshot_bytes(&dest);

    let project_path = fx.project_legacy_path();
    fs::create_dir_all(project_path.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("/etc/hostname", &project_path).unwrap();
    let legacy_before = snapshot_bytes(&project_path);

    let migrator = fx.migrator();
    let result = migrator
        .migrate(fx.request(vec![fx.legacy_input(LegacyKind::ProjectLocal)]))
        .await;
    assert!(result.is_err());

    assert_eq!(
        snapshot_bytes(&dest),
        dest_before,
        "valid destination preserved"
    );
    assert_eq!(
        snapshot_bytes(&project_path),
        legacy_before,
        "symlink legacy preserved"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_dest_not_accepted_before_active_workspace() {
    // AC3: a valid destination exists but a legacy input is under an active
    // workspace.
    let fx = Fixture::new().await;
    let dest = fx.destination();
    make_clean_checkout(&fx.mirror_path, &dest);
    let dest_before = snapshot_bytes(&dest);

    let project_path = fx.project_legacy_path();
    make_clean_checkout(&fx.mirror_path, &project_path);
    let legacy_before = snapshot_bytes(&project_path);

    fx.db.ensure_initialized().await.unwrap();
    let task_id = seed_task_row(
        &fx.db,
        UsageTestTaskSeed {
            project_id: "owner-proj-001",
            status: "open",
            close_reason: None,
            total_reopen_count: 0,
        },
    )
    .await;
    let workspace = fx.owner_root.to_string_lossy().to_string();
    TaskRunRepository::new(fx.db.clone())
        .create(CreateTaskRunParams {
            id: "active-run-2",
            project_id: "owner-proj-001",
            task_id: &task_id,
            trigger_type: "new_task",
            status: Some("running"),
            workspace_path: Some(&workspace),
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();

    let migrator = fx.migrator();
    let result = migrator
        .migrate(fx.request(vec![fx.legacy_input(LegacyKind::ProjectLocal)]))
        .await;
    assert!(result.is_err());

    assert_eq!(
        snapshot_bytes(&dest),
        dest_before,
        "valid destination preserved"
    );
    assert_eq!(
        snapshot_bytes(&project_path),
        legacy_before,
        "active legacy preserved"
    );
}

// ── AC4: Publication boundary — finalization fails on a published dest ──

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finalization_failure_on_published_destination_remains_pending() {
    // AC4: the normal missing-destination publication boundary where
    // finalization fails. The destination is published but the record stays
    // pending. Restart reconciliation finalizes only a validated destination.
    let fx = Fixture::new().await;
    let mut request = fx.request(vec![]);
    request.fail_at = Some(MigrationFailurePoint::FailFinalize);

    let migrator = fx.migrator();
    let result = migrator.migrate(request).await;
    assert!(result.is_err(), "injected finalize must fail");

    // The destination was published even though finalization failed.
    let dest = fx.destination();
    assert_eq!(
        classify(&dest, &fx.target_commit),
        ReadSourcePathState::Clean {
            commit: fx.target_commit.clone()
        }
    );

    // Record must be pending.
    let repo = ProjectLiveStateMigrationRepository::new(fx.db.clone());
    let (owner, family) = fx.migration_key();
    let record = repo
        .get(fx.make_key(&owner, &family))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.result, "pending");

    let dest_before = snapshot_bytes(&dest);

    // Restart: reconcile should finalize the validated destination.
    let result = migrator.reconcile(fx.request(vec![])).await.unwrap();
    assert!(matches!(result, ReadSourceMigrationResult::Existing(_)));

    let record = repo
        .get(fx.make_key(&owner, &family))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.result, "succeeded");

    // Destination unchanged after reconciliation.
    assert_eq!(
        snapshot_bytes(&dest),
        dest_before,
        "destination unchanged after restart"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_reconcile_does_not_finalize_dirty_destination() {
    // AC4: if the destination becomes dirty between publication and restart,
    // reconciliation must NOT finalize it. It fails closed.
    let fx = Fixture::new().await;
    let mut request = fx.request(vec![]);
    request.fail_at = Some(MigrationFailurePoint::FailFinalize);

    let migrator = fx.migrator();
    let _ = migrator.migrate(request).await;
    let dest = fx.destination();
    assert!(dest.exists());

    // Tamper with the destination so it's no longer clean.
    fs::write(dest.join("README.md"), "tampered\n").unwrap();

    // Reconciliation must fail — destination is no longer valid.
    let result = migrator.reconcile(fx.request(vec![])).await;
    assert!(
        result.is_err(),
        "reconcile must fail closed on dirty destination"
    );
}

// ── AC5: Same-owner same-target sharing across separate task attempts ──

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_owner_same_target_sharing_across_attempts() {
    // AC5: two separate migration attempts for the same (owner, target)
    // share the same cache. The first publishes, the second accepts existing.
    let fx = Fixture::new().await;
    let dest = fx.destination();
    let migrator = fx.migrator();

    // First attempt: publish.
    let r1 = migrator.migrate(fx.request(vec![])).await.unwrap();
    assert!(matches!(r1, ReadSourceMigrationResult::Published(_)));
    let dest_after_first = snapshot_bytes(&dest);

    // Second attempt: accept existing.
    let r2 = migrator.migrate(fx.request(vec![])).await.unwrap();
    assert!(matches!(r2, ReadSourceMigrationResult::Existing(_)));
    let dest_after_second = snapshot_bytes(&dest);

    assert_eq!(
        dest_after_first, dest_after_second,
        "destination is shared idempotently"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deterministic_rollback_after_pending_publish() {
    // AC5: deterministic rollback/restart reconciliation after a published
    // but pending destination.
    let fx = Fixture::new().await;
    let dest = fx.destination();

    // Publish with finalize injection.
    let mut request = fx.request(vec![]);
    request.fail_at = Some(MigrationFailurePoint::FailFinalize);
    let migrator = fx.migrator();
    let _ = migrator.migrate(request).await;
    let dest_before = snapshot_bytes(&dest);

    // Rollback retains the valid destination.
    let (owner, _) = fx.migration_key();
    migrator
        .rollback(&owner, &fx.target_project_id, &fx.owner_root)
        .await
        .unwrap();

    assert_eq!(
        snapshot_bytes(&dest),
        dest_before,
        "rollback retains valid destination byte-for-byte"
    );

    let repo = ProjectLiveStateMigrationRepository::new(fx.db.clone());
    let (owner, family) = fx.migration_key();
    let record = repo
        .get(fx.make_key(&owner, &family))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.result, "rolled_back");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn different_targets_isolated_under_same_owner() {
    // AC2/AC5: different targets under the same owner remain isolated.
    let fx = Fixture::new().await;
    let migrator = fx.migrator();

    let mut req_a = fx.request(vec![]);
    req_a.target_project_id = "target-isolated-a".to_string();
    migrator.migrate(req_a).await.unwrap();

    let mut req_b = fx.request(vec![]);
    req_b.target_project_id = "target-isolated-b".to_string();
    migrator.migrate(req_b).await.unwrap();

    let dest_a = ReadSourceMigrator::destination_for(&fx.owner_root, "target-isolated-a");
    let dest_b = ReadSourceMigrator::destination_for(&fx.owner_root, "target-isolated-b");
    assert!(dest_a.exists());
    assert!(dest_b.exists());
    assert_ne!(dest_a, dest_b);
    // Each destination is clean at its own commit.
    let commit_a = git(&dest_a, &["rev-parse", "HEAD"]).unwrap();
    let commit_b = git(&dest_b, &["rev-parse", "HEAD"]).unwrap();
    assert_eq!(commit_a, commit_b, "same mirror → same commit");
    assert_eq!(
        classify(&dest_a, &commit_a),
        ReadSourcePathState::Clean { commit: commit_a }
    );
    assert_eq!(
        classify(&dest_b, &commit_b),
        ReadSourcePathState::Clean { commit: commit_b }
    );
}

// ── AC3: Valid destination preservation for every ambiguous boundary ────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_dest_not_accepted_before_invalid_git_legacy() {
    let fx = Fixture::new().await;
    let dest = fx.destination();
    make_clean_checkout(&fx.mirror_path, &dest);

    let project_path = fx.project_legacy_path();
    fs::create_dir_all(project_path.join(".git")).unwrap();
    fs::write(project_path.join("README.md"), "partial checkout\n").unwrap();
    let dest_before = snapshot_bytes(&dest);
    let legacy_before = snapshot_bytes(&project_path);

    let result = fx
        .migrator()
        .migrate(fx.request(vec![fx.legacy_input(LegacyKind::ProjectLocal)]))
        .await;
    let error = result.expect_err("invalid git legacy must fail closed");
    assert!(
        matches!(&error, ReadSourceMigrationError::Ambiguous(detail) if detail.contains("invalid_git")),
        "expected invalid_git classification, got: {error}"
    );
    assert_eq!(
        classify(&project_path, &fx.target_commit),
        ReadSourcePathState::InvalidGit
    );
    assert_eq!(
        snapshot_bytes(&dest),
        dest_before,
        "valid destination preserved"
    );
    assert_eq!(
        snapshot_bytes(&project_path),
        legacy_before,
        "invalid_git legacy including .git bytes preserved"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_dest_not_accepted_before_special_legacy() {
    let fx = Fixture::new().await;
    let dest = fx.destination();
    make_clean_checkout(&fx.mirror_path, &dest);

    let project_path = fx.project_legacy_path();
    fs::create_dir_all(project_path.parent().unwrap()).unwrap();
    let status = std::process::Command::new("mkfifo")
        .arg(&project_path)
        .status()
        .expect("mkfifo available on unix");
    assert!(status.success());
    let dest_before = snapshot_bytes(&dest);
    let legacy_before = snapshot_bytes(&project_path);

    let result = fx
        .migrator()
        .migrate(fx.request(vec![fx.legacy_input(LegacyKind::ProjectLocal)]))
        .await;
    let error = result.expect_err("special legacy must fail closed");
    assert!(
        matches!(&error, ReadSourceMigrationError::Ambiguous(detail) if detail.contains("special")),
        "expected special classification, got: {error}"
    );
    assert_eq!(
        classify(&project_path, &fx.target_commit),
        ReadSourcePathState::Special
    );
    assert_eq!(
        snapshot_bytes(&dest),
        dest_before,
        "valid destination preserved"
    );
    assert_eq!(
        snapshot_bytes(&project_path),
        legacy_before,
        "special legacy preserves exact no-follow identity"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_dest_not_accepted_when_liveness_query_is_uncertain() {
    let fx = Fixture::new().await;
    let dest = fx.destination();
    make_clean_checkout(&fx.mirror_path, &dest);
    let project_path = fx.project_legacy_path();
    make_clean_checkout(&fx.mirror_path, &project_path);
    let dest_before = snapshot_bytes(&dest);
    let legacy_before = snapshot_bytes(&project_path);

    fx.db.ensure_initialized().await.unwrap();
    drop_table_cascade_for_test(&fx.db, "task_runs").await;

    let result = fx
        .migrator()
        .migrate(fx.request(vec![fx.legacy_input(LegacyKind::ProjectLocal)]))
        .await;
    assert!(
        matches!(result, Err(ReadSourceMigrationError::Database(_))),
        "liveness-query uncertainty must return its typed database error"
    );
    assert_eq!(
        snapshot_bytes(&dest),
        dest_before,
        "valid destination preserved"
    );
    assert_eq!(
        snapshot_bytes(&project_path),
        legacy_before,
        "legacy preserved while liveness is uncertain"
    );
}
