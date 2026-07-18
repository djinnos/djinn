use super::*;
use djinn_db::{
    CreateTaskRunParams, Database, TaskRunRepository,
    test_support::{UsageTestTaskSeed, drop_table_cascade_for_test, seed_project, seed_task_row},
};
use std::os::unix::fs::{FileTypeExt, MetadataExt};

/// Create a bare git repository at `work/mirror.git` with one commit.
/// Returns the commit SHA.
fn init_bare_mirror(work: &Path) -> String {
    let source = work.join("source-repo");
    fs::create_dir_all(&source).unwrap();
    run_git(&source, &["init"]);
    run_git(&source, &["config", "user.email", "test@test.com"]);
    run_git(&source, &["config", "user.name", "Test"]);
    fs::write(source.join("README.md"), "hello\n").unwrap();
    run_git(&source, &["add", "."]);
    run_git(&source, &["commit", "-m", "initial"]);

    let mirror = work.join("mirror.git");
    run_git(
        work,
        &[
            "clone",
            "--bare",
            source.to_str().unwrap(),
            mirror.to_str().unwrap(),
        ],
    );
    git(&mirror, &["rev-parse", "HEAD"]).unwrap()
}

fn run_git(dir: &Path, args: &[&str]) {
    djinn_git::run_git_command_binary_in(dir, args.iter().map(|arg| (*arg).to_owned()).collect())
        .unwrap_or_else(|error| {
            panic!(
                "git {} failed in {}: {error}",
                args.join(" "),
                dir.display()
            )
        });
}

/// Create a clean detached checkout of the mirror at `dest`.
fn make_clean_checkout(mirror: &Path, dest: &Path) {
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    run_git(
        dest.parent().unwrap(),
        &[
            "clone",
            "--local",
            "--shared",
            "--no-checkout",
            mirror.to_str().unwrap(),
            dest.to_str().unwrap(),
        ],
    );
    run_git(dest, &["checkout", "--detach", "HEAD"]);
}

fn test_db() -> Database {
    Database::open_in_memory().expect("open test database")
}

/// A complete test fixture: an owner root with a bare mirror and the
/// expected legacy directory structure.
struct Fixture {
    _tmp: tempfile::TempDir,
    tmp_path: PathBuf,
    db: Database,
    mirror_path: PathBuf,
    owner_root: PathBuf,
    target_project_id: String,
    target_commit: String,
}

impl Fixture {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_path = tmp.path().to_path_buf();
        let target_commit = init_bare_mirror(&tmp_path);
        let mirror_path = tmp_path.join("mirror.git");
        let owner_root = tmp_path.join("owner-root");
        fs::create_dir_all(&owner_root).unwrap();
        let db = test_db();
        seed_project(&db, "owner-proj-001", "owner").await;
        let target_project_id = "target-proj-001".to_string();
        Self {
            _tmp: tmp,
            tmp_path,
            db,
            mirror_path,
            owner_root,
            target_project_id,
            target_commit,
        }
    }

    fn migrator(&self) -> ReadSourceMigrator {
        ReadSourceMigrator::new(self.db.clone())
    }

    fn request(&self, legacy_inputs: Vec<LegacyReadSource>) -> ReadSourceMigrationRequest {
        ReadSourceMigrationRequest {
            owner_project_id: "owner-proj-001".to_string(),
            target_project_id: self.target_project_id.clone(),
            owner_root: self.owner_root.clone(),
            mirror_path: self.mirror_path.clone(),
            legacy_inputs,
            fail_at: None,
        }
    }

    fn project_legacy_path(&self) -> PathBuf {
        self.owner_root
            .join(".djinn/read-sources")
            .join(&self.target_project_id)
    }

    fn destination(&self) -> PathBuf {
        ReadSourceMigrator::destination_for(&self.owner_root, &self.target_project_id)
    }

    fn legacy_input(&self, kind: LegacyKind) -> LegacyReadSource {
        let path = match kind {
            LegacyKind::ProjectLocal => self.project_legacy_path(),
            LegacyKind::TaskLocal => self
                .owner_root
                .join("workspace/.djinn-read-sources")
                .join(&self.target_project_id),
        };
        LegacyReadSource { kind, path }
    }

    fn migration_key(&self) -> (String, String) {
        (
            "owner-proj-001".to_string(),
            format!("read_source:{}", self.target_project_id),
        )
    }

    /// Construct a fresh MigrationKey borrowing from the provided owner
    /// and family strings. Call this for each DB call since MigrationKey
    /// does not implement Copy/Clone.
    fn make_key<'a>(&'a self, owner: &'a str, family: &'a str) -> MigrationKey<'a> {
        MigrationKey {
            project_id: owner,
            family,
            release: RELEASE,
        }
    }
}

// ── AC1: Named state classes ──────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn classify_absent_input() {
    // No legacy inputs exist, no destination. Migration should publish
    // a clean detached destination.
    let fx = Fixture::new().await;
    let migrator = fx.migrator();
    let result = migrator.migrate(fx.request(vec![])).await.unwrap();
    assert!(matches!(result, ReadSourceMigrationResult::Published(_)));
    let dest = fx.destination();
    assert_eq!(
        classify(&dest, &fx.target_commit),
        ReadSourcePathState::Clean {
            commit: fx.target_commit.clone()
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn classify_identical_clean_dual_inputs() {
    // Two clean legacy inputs at the same commit → migration succeeds.
    let fx = Fixture::new().await;
    let project_path = fx.project_legacy_path();
    make_clean_checkout(&fx.mirror_path, &project_path);
    let task_path = fx
        .owner_root
        .join("workspace/.djinn-read-sources")
        .join(&fx.target_project_id);
    fs::create_dir_all(task_path.parent().unwrap()).unwrap();
    make_clean_checkout(&fx.mirror_path, &task_path);

    let migrator = fx.migrator();
    let result = migrator
        .migrate(fx.request(vec![
            fx.legacy_input(LegacyKind::ProjectLocal),
            fx.legacy_input(LegacyKind::TaskLocal),
        ]))
        .await
        .unwrap();
    assert!(matches!(result, ReadSourceMigrationResult::Published(_)));
    // Both legacy inputs preserved.
    assert_eq!(
        classify(&project_path, &fx.target_commit),
        ReadSourcePathState::Clean {
            commit: fx.target_commit.clone()
        }
    );
    assert_eq!(
        classify(&task_path, &fx.target_commit),
        ReadSourcePathState::Clean {
            commit: fx.target_commit.clone()
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn classify_dirty_tracked_content() {
    let fx = Fixture::new().await;
    let project_path = fx.project_legacy_path();
    make_clean_checkout(&fx.mirror_path, &project_path);
    // Modify a tracked file.
    fs::write(project_path.join("README.md"), "dirty\n").unwrap();

    let migrator = fx.migrator();
    let result = migrator
        .migrate(fx.request(vec![fx.legacy_input(LegacyKind::ProjectLocal)]))
        .await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(&err, ReadSourceMigrationError::Ambiguous(d) if d.contains("dirty_tracked")),
        "expected dirty_tracked failure, got: {err}"
    );
    // Legacy input preserved.
    assert_eq!(
        classify(&project_path, &fx.target_commit),
        ReadSourcePathState::DirtyTracked
    );
    // Destination must NOT exist.
    assert!(!fx.destination().exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn classify_untracked_content() {
    let fx = Fixture::new().await;
    let project_path = fx.project_legacy_path();
    make_clean_checkout(&fx.mirror_path, &project_path);
    // Add an untracked file.
    fs::write(project_path.join("untracked.txt"), "stuff\n").unwrap();

    let migrator = fx.migrator();
    let result = migrator
        .migrate(fx.request(vec![fx.legacy_input(LegacyKind::ProjectLocal)]))
        .await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(&err, ReadSourceMigrationError::Ambiguous(d) if d.contains("untracked")),
        "expected untracked failure, got: {err}"
    );
    assert_eq!(
        classify(&project_path, &fx.target_commit),
        ReadSourcePathState::Untracked
    );
    assert!(!fx.destination().exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn classify_ignored_content() {
    let fx = Fixture::new().await;
    let project_path = fx.project_legacy_path();
    make_clean_checkout(&fx.mirror_path, &project_path);
    // Add an ignored file via .git/info/exclude.
    fs::write(project_path.join(".git/info/exclude"), "*.ignored\n").unwrap();
    fs::write(project_path.join("file.ignored"), "ignored\n").unwrap();

    let migrator = fx.migrator();
    let result = migrator
        .migrate(fx.request(vec![fx.legacy_input(LegacyKind::ProjectLocal)]))
        .await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(&err, ReadSourceMigrationError::Ambiguous(d) if d.contains("ignored")),
        "expected ignored failure, got: {err}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn classify_differing_dual_inputs() {
    let fx = Fixture::new().await;
    let project_path = fx.project_legacy_path();
    make_clean_checkout(&fx.mirror_path, &project_path);

    // Create a second mirror with a different commit.
    let source2 = fx.tmp_path.join("source2-repo");
    fs::create_dir_all(&source2).unwrap();
    run_git(&source2, &["init"]);
    run_git(&source2, &["config", "user.email", "t@t.com"]);
    run_git(&source2, &["config", "user.name", "T"]);
    fs::write(source2.join("README.md"), "different\n").unwrap();
    run_git(&source2, &["add", "."]);
    run_git(&source2, &["commit", "-m", "other"]);
    let mirror2 = fx.tmp_path.join("mirror2.git");
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

    let migrator = fx.migrator();
    let result = migrator
        .migrate(fx.request(vec![
            fx.legacy_input(LegacyKind::ProjectLocal),
            fx.legacy_input(LegacyKind::TaskLocal),
        ]))
        .await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(&err, ReadSourceMigrationError::Ambiguous(d) if d.contains("differing")),
        "expected differing failure, got: {err}"
    );
    // Both inputs preserved.
    assert!(project_path.exists());
    assert!(task_path.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn classify_unknown_parent_entry() {
    let fx = Fixture::new().await;
    // Put an unexpected entry in the legacy parent.
    let legacy_parent = fx.owner_root.join(".djinn/read-sources");
    fs::create_dir_all(&legacy_parent).unwrap();
    fs::create_dir_all(legacy_parent.join("unexpected-target")).unwrap();

    let migrator = fx.migrator();
    let result = migrator
        .migrate(fx.request(vec![fx.legacy_input(LegacyKind::ProjectLocal)]))
        .await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(err, ReadSourceMigrationError::UnknownEntry { .. }),
        "expected UnknownEntry, got: {err}"
    );
    // The unknown entry is preserved.
    assert!(legacy_parent.join("unexpected-target").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn classify_symlink_input() {
    let fx = Fixture::new().await;
    let project_path = fx.project_legacy_path();
    fs::create_dir_all(project_path.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("/etc/hostname", &project_path).unwrap();

    let migrator = fx.migrator();
    let result = migrator
        .migrate(fx.request(vec![fx.legacy_input(LegacyKind::ProjectLocal)]))
        .await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(&err, ReadSourceMigrationError::Ambiguous(d) if d.contains("symlink")),
        "expected symlink failure, got: {err}"
    );
    // Symlink preserved.
    assert!(project_path.is_symlink());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn classify_file_where_dir_expected() {
    let fx = Fixture::new().await;
    let project_path = fx.project_legacy_path();
    fs::create_dir_all(project_path.parent().unwrap()).unwrap();
    fs::write(&project_path, "not a directory\n").unwrap();

    let migrator = fx.migrator();
    let result = migrator
        .migrate(fx.request(vec![fx.legacy_input(LegacyKind::ProjectLocal)]))
        .await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(&err, ReadSourceMigrationError::Ambiguous(d) if d.contains("file")),
        "expected file failure, got: {err}"
    );
    // File preserved.
    assert!(project_path.is_file());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn classify_identity_mismatched_destination() {
    let fx = Fixture::new().await;
    // Create a destination at a different commit.
    let source2 = fx.tmp_path.join("source-mismatch");
    fs::create_dir_all(&source2).unwrap();
    run_git(&source2, &["init"]);
    run_git(&source2, &["config", "user.email", "t@t.com"]);
    run_git(&source2, &["config", "user.name", "T"]);
    fs::write(source2.join("README.md"), "other\n").unwrap();
    run_git(&source2, &["add", "."]);
    run_git(&source2, &["commit", "-m", "other"]);
    let mirror2 = fx.tmp_path.join("mirror-mismatch.git");
    run_git(
        &fx.tmp_path,
        &[
            "clone",
            "--bare",
            source2.to_str().unwrap(),
            mirror2.to_str().unwrap(),
        ],
    );
    let dest = fx.destination();
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    make_clean_checkout(&mirror2, &dest);

    let migrator = fx.migrator();
    let result = migrator.migrate(fx.request(vec![])).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(&err, ReadSourceMigrationError::Ambiguous(d) if d.contains("identity_mismatch")),
        "expected identity_mismatch failure, got: {err}"
    );
    // Destination preserved byte-for-byte.
    assert!(dest.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn classify_dirty_destination() {
    let fx = Fixture::new().await;
    let dest = fx.destination();
    make_clean_checkout(&fx.mirror_path, &dest);
    // Dirty it.
    fs::write(dest.join("README.md"), "dirty\n").unwrap();

    let migrator = fx.migrator();
    let result = migrator.migrate(fx.request(vec![])).await;
    assert!(result.is_err());
    // Destination preserved.
    assert!(dest.exists());
    assert_eq!(
        classify(&dest, &fx.target_commit),
        ReadSourcePathState::DirtyTracked
    );
}

// ── AC2: Fail-closed and preserve inputs ──────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_destination_plus_dirty_legacy_fails_closed() {
    // The core AC2 scenario: a valid destination exists, but a legacy
    // input is dirty. The engine must NOT accept the destination; it must
    // fail closed.
    let fx = Fixture::new().await;
    let dest = fx.destination();
    make_clean_checkout(&fx.mirror_path, &dest);

    let project_path = fx.project_legacy_path();
    make_clean_checkout(&fx.mirror_path, &project_path);
    fs::write(project_path.join("README.md"), "dirty\n").unwrap();
    let dest_before = snapshot_bytes(&dest);
    let legacy_before = snapshot_bytes(&project_path);

    let migrator = fx.migrator();
    let result = migrator
        .migrate(fx.request(vec![fx.legacy_input(LegacyKind::ProjectLocal)]))
        .await;
    assert!(result.is_err(), "must fail closed with dirty legacy input");
    assert_eq!(
        classify(&dest, &fx.target_commit),
        ReadSourcePathState::Clean {
            commit: fx.target_commit.clone()
        }
    );
    assert_eq!(
        classify(&project_path, &fx.target_commit),
        ReadSourcePathState::DirtyTracked
    );
    assert_eq!(
        snapshot_bytes(&dest),
        dest_before,
        "valid destination preserved byte-for-byte"
    );
    assert_eq!(
        snapshot_bytes(&project_path),
        legacy_before,
        "dirty legacy preserved byte-for-byte"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_destination_with_clean_legacy_accepts_existing() {
    // Happy path: clean legacy + clean destination → accept existing.
    let fx = Fixture::new().await;
    let dest = fx.destination();
    make_clean_checkout(&fx.mirror_path, &dest);
    let project_path = fx.project_legacy_path();
    make_clean_checkout(&fx.mirror_path, &project_path);

    let migrator = fx.migrator();
    let result = migrator
        .migrate(fx.request(vec![fx.legacy_input(LegacyKind::ProjectLocal)]))
        .await
        .unwrap();
    assert!(matches!(result, ReadSourceMigrationResult::Existing(_)));
    assert_eq!(
        classify(&dest, &fx.target_commit),
        ReadSourcePathState::Clean {
            commit: fx.target_commit.clone()
        }
    );
}

// ── AC3: Durable records and lock ordering ─────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_mirror_emits_durable_failure_record() {
    // AC3: even a pre-decision failure (invalid mirror) must produce a
    // durable record while the lock is held.
    let fx = Fixture::new().await;
    let legacy_input = fx.legacy_input(LegacyKind::TaskLocal);
    let legacy_path = legacy_input.path.display().to_string();
    let mut request = fx.request(vec![legacy_input]);
    request.mirror_path = fx.tmp_path.join("nonexistent.git");

    let migrator = fx.migrator();
    let result = migrator.migrate(request).await;
    assert!(result.is_err());

    // A durable failure record must exist.
    let repo = ProjectLiveStateMigrationRepository::new(fx.db.clone());
    let (owner, family) = fx.migration_key();
    let key = MigrationKey {
        project_id: &owner,
        family: &family,
        release: RELEASE,
    };
    let record = repo.get(key).await.unwrap().expect("durable record exists");
    assert_eq!(record.result, "failed");
    assert!(
        record.source_inventory["sources"]
            .as_array()
            .expect("failure inventory has sources")
            .iter()
            .any(|source| source["path"].as_str() == Some(&legacy_path))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ambiguous_state_emits_durable_failure_record() {
    let fx = Fixture::new().await;
    let project_path = fx.project_legacy_path();
    make_clean_checkout(&fx.mirror_path, &project_path);
    fs::write(project_path.join("README.md"), "dirty\n").unwrap();

    let migrator = fx.migrator();
    let _ = migrator
        .migrate(fx.request(vec![fx.legacy_input(LegacyKind::ProjectLocal)]))
        .await;

    let repo = ProjectLiveStateMigrationRepository::new(fx.db.clone());
    let (owner, family) = fx.migration_key();
    let key = MigrationKey {
        project_id: &owner,
        family: &family,
        release: RELEASE,
    };
    let record = repo.get(key).await.unwrap().expect("durable record exists");
    assert_eq!(record.result, "failed");
    // Inventory must contain structured multi-source data.
    let sources = record.source_inventory["sources"]
        .as_array()
        .expect("sources array");
    assert!(!sources.is_empty());
    assert_eq!(sources[0]["state"].as_str().unwrap(), "dirty_tracked");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successful_migration_emits_finalized_record() {
    let fx = Fixture::new().await;
    let migrator = fx.migrator();
    migrator.migrate(fx.request(vec![])).await.unwrap();

    let repo = ProjectLiveStateMigrationRepository::new(fx.db.clone());
    let (owner, family) = fx.migration_key();
    let key = MigrationKey {
        project_id: &owner,
        family: &family,
        release: RELEASE,
    };
    let record = repo.get(key).await.unwrap().expect("durable record exists");
    assert_eq!(record.result, "succeeded");
    assert_eq!(record.post_hash.as_deref(), Some(fx.target_commit.as_str()));
    assert!(record.finalized_at.is_some());
    // Rollback instruction present.
    assert!(!record.rollback_instruction.is_empty());
}

// ── AC3: Active workspace / liveness-query uncertainty ────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_workspace_fails_closed() {
    // Seed a live task_run whose workspace_path contains a legacy input.
    let fx = Fixture::new().await;
    let project_path = fx.project_legacy_path();
    make_clean_checkout(&fx.mirror_path, &project_path);

    // Create a live task_run under the owner project with a workspace
    // that contains the legacy input path.
    fx.db.ensure_initialized().await.unwrap();
    let workspace = fx.owner_root.to_string_lossy().to_string();
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
    TaskRunRepository::new(fx.db.clone())
        .create(CreateTaskRunParams {
            id: "active-run",
            project_id: "owner-proj-001",
            task_id: &task_id,
            trigger_type: "new_task",
            status: Some("running"),
            workspace_path: Some(&workspace),
            mirror_ref: None,
        })
        .await
        .unwrap();

    let migrator = fx.migrator();
    let result = migrator
        .migrate(fx.request(vec![fx.legacy_input(LegacyKind::ProjectLocal)]))
        .await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(err, ReadSourceMigrationError::ActiveWorkspace(_)),
        "expected ActiveWorkspace, got: {err}"
    );
    // Legacy input preserved.
    assert_eq!(
        classify(&project_path, &fx.target_commit),
        ReadSourcePathState::Clean {
            commit: fx.target_commit.clone()
        }
    );
    // Destination must NOT exist.
    assert!(!fx.destination().exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn db_liveness_query_uncertainty_fails_closed() {
    // Drop the task_runs table to simulate DB uncertainty.
    let fx = Fixture::new().await;
    fx.db.ensure_initialized().await.unwrap();
    drop_table_cascade_for_test(&fx.db, "task_runs").await;

    let migrator = fx.migrator();
    let result = migrator.migrate(fx.request(vec![])).await;
    assert!(result.is_err(), "DB uncertainty must fail closed");
}

// ── AC4: Same-owner cache sharing & different-owner/target isolation ──

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_owner_shares_cache_across_targets() {
    let fx = Fixture::new().await;
    let migrator = fx.migrator();

    // Migrate target A.
    let mut req_a = fx.request(vec![]);
    req_a.target_project_id = "target-a".to_string();
    migrator.migrate(req_a).await.unwrap();

    // Migrate target B (same owner).
    let mut req_b = fx.request(vec![]);
    req_b.target_project_id = "target-b".to_string();
    migrator.migrate(req_b).await.unwrap();

    // Both destinations exist under the same owner root.
    let dest_a = ReadSourceMigrator::destination_for(&fx.owner_root, "target-a");
    let dest_b = ReadSourceMigrator::destination_for(&fx.owner_root, "target-b");
    assert!(dest_a.exists());
    assert!(dest_b.exists());
    assert_ne!(dest_a, dest_b);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn different_owners_are_isolated() {
    let fx = Fixture::new().await;
    let migrator = fx.migrator();

    // Owner one.
    migrator.migrate(fx.request(vec![])).await.unwrap();

    // Owner two — different root and a distinct durable project identity.
    seed_project(&fx.db, "owner-proj-002", "other-owner").await;
    let owner2_root = fx.tmp_path.join("owner2-root");
    fs::create_dir_all(&owner2_root).unwrap();
    let mut req2 = fx.request(vec![]);
    req2.owner_project_id = "owner-proj-002".to_string();
    req2.owner_root = owner2_root.clone();
    migrator.migrate(req2).await.unwrap();

    let dest1 = ReadSourceMigrator::destination_for(&fx.owner_root, &fx.target_project_id);
    let dest2 = ReadSourceMigrator::destination_for(&owner2_root, &fx.target_project_id);
    assert!(dest1.exists());
    assert!(dest2.exists());
    assert_ne!(dest1, dest2);
}

// ── AC4: Concurrent lock behavior ─────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_same_project_migration_is_serialized() {
    let fx = Fixture::new().await;
    let migrator = ReadSourceMigrator::new(fx.db.clone());
    let request = fx.request(vec![]);

    // Hold the lock manually.
    let runtime = fx.owner_root.join(".task-runtime");
    fs::create_dir_all(&runtime).unwrap();
    let _lock = ProjectLiveStateMigrationLock::try_acquire(&runtime, "owner-proj-001").unwrap();

    let result = migrator.migrate(request).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(
            err,
            ReadSourceMigrationError::LiveState(
                djinn_core::live_state_migration::LiveStateMigrationError::LockHeld { .. }
            )
        ),
        "expected LockHeld, got: {err}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconcile_and_rollback_cannot_touch_active_staging_before_lock() {
    let fx = Fixture::new().await;
    let destination = fx.destination();
    let parent = destination.parent().unwrap();
    fs::create_dir_all(parent).unwrap();
    let staging = ReadSourceMigrator::staging_path(parent, &fx.target_project_id);
    fs::create_dir_all(&staging).unwrap();
    fs::write(staging.join("active-marker"), "active migration\n").unwrap();

    // Model an in-flight migration that owns the deterministic staging tree.
    let runtime = fx.owner_root.join(".task-runtime");
    fs::create_dir_all(&runtime).unwrap();
    let lock = ProjectLiveStateMigrationLock::try_acquire(&runtime, "owner-proj-001").unwrap();
    let migrator = fx.migrator();
    let (reconcile, rollback) = tokio::join!(
        migrator.reconcile(fx.request(vec![])),
        migrator.rollback("owner-proj-001", &fx.target_project_id, &fx.owner_root),
    );
    assert!(matches!(
        reconcile,
        Err(ReadSourceMigrationError::LiveState(_))
    ));
    assert!(matches!(
        rollback,
        Err(ReadSourceMigrationError::LiveState(_))
    ));
    assert_eq!(
        fs::read_to_string(staging.join("active-marker")).unwrap(),
        "active migration\n",
        "contenders must not delete or alter staging before acquiring the lock"
    );
    drop(lock);

    // After the owner releases the lock, reconciliation may remove the
    // abandoned staging tree and publish the cache.
    migrator.reconcile(fx.request(vec![])).await.unwrap();
    assert!(destination.exists());
    assert!(!staging.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconcile_symlink_staging_records_durable_failure() {
    let fx = Fixture::new().await;
    let destination = fx.destination();
    let parent = destination.parent().unwrap();
    fs::create_dir_all(parent).unwrap();
    let staging = ReadSourceMigrator::staging_path(parent, &fx.target_project_id);
    std::os::unix::fs::symlink("unexpected-target", &staging).unwrap();

    let migrator = fx.migrator();
    let result = migrator.reconcile(fx.request(vec![])).await;
    assert!(matches!(
        result,
        Err(ReadSourceMigrationError::Ambiguous(_))
    ));

    let repo = ProjectLiveStateMigrationRepository::new(fx.db.clone());
    let (owner, family) = fx.migration_key();
    let record = repo
        .get(fx.make_key(&owner, &family))
        .await
        .unwrap()
        .expect("reconcile attempt is durable");
    assert_eq!(record.result, "failed");
    assert!(
        record
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("symlink"),
        "inspectable staging failure must be recorded before returning"
    );
    assert!(
        staging.is_symlink(),
        "ambiguous staging symlink is retained"
    );
}

// ── AC4: Injected clone/rename/finalization failure ───────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injected_clone_failure_preserves_inputs() {
    let fx = Fixture::new().await;
    let mut request = fx.request(vec![]);
    request.fail_at = Some(MigrationFailurePoint::FailClone);

    let migrator = fx.migrator();
    let result = migrator.migrate(request).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(
            err,
            ReadSourceMigrationError::InjectedFailure(MigrationFailurePoint::FailClone)
        ),
        "expected FailClone, got: {err}"
    );
    // Destination must NOT exist.
    assert!(!fx.destination().exists());
    // No stale temp left behind (AC4 restart reconciliation).
    let dest = fx.destination();
    let parent = dest.parent().unwrap();
    let temp = parent.join(format!(
        ".{}.read-source-migration.{}",
        fx.target_project_id,
        std::process::id()
    ));
    assert!(
        !temp.exists(),
        "stale temp must be cleaned up after clone failure"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injected_rename_failure_preserves_destination() {
    let fx = Fixture::new().await;
    let mut request = fx.request(vec![]);
    request.fail_at = Some(MigrationFailurePoint::FailRename);

    let migrator = fx.migrator();
    let result = migrator.migrate(request).await;
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        ReadSourceMigrationError::InjectedFailure(MigrationFailurePoint::FailRename)
    ));
    // Destination must NOT exist (rename never happened).
    assert!(!fx.destination().exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injected_checkout_failure_preserves_inputs() {
    let fx = Fixture::new().await;
    let mut request = fx.request(vec![]);
    request.fail_at = Some(MigrationFailurePoint::FailCheckout);

    let migrator = fx.migrator();
    let result = migrator.migrate(request).await;
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        ReadSourceMigrationError::InjectedFailure(MigrationFailurePoint::FailCheckout)
    ));
    assert!(!fx.destination().exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injected_finalization_failure_leaves_pending_record() {
    let fx = Fixture::new().await;
    let dest = fx.destination();
    make_clean_checkout(&fx.mirror_path, &dest);
    let mut request = fx.request(vec![]);
    request.fail_at = Some(MigrationFailurePoint::FailFinalize);

    let migrator = fx.migrator();
    let result = migrator.migrate(request).await;
    assert!(result.is_err());

    // The record should be pending (not finalized).
    let repo = ProjectLiveStateMigrationRepository::new(fx.db.clone());
    let (owner, family) = fx.migration_key();
    let key = MigrationKey {
        project_id: &owner,
        family: &family,
        release: RELEASE,
    };
    let record = repo.get(key).await.unwrap().expect("record exists");
    assert_eq!(record.result, "pending");
    // Destination preserved.
    assert_eq!(
        classify(&dest, &fx.target_commit),
        ReadSourcePathState::Clean {
            commit: fx.target_commit.clone()
        }
    );
}

// ── AC4: Restart reconciliation ───────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconcile_recovers_from_leftover_staging_temp() {
    let fx = Fixture::new().await;
    let dest = fx.destination();
    let parent = dest.parent().unwrap();
    fs::create_dir_all(parent).unwrap();
    let temp = parent.join(format!(
        ".{}.read-source-migration.{}",
        fx.target_project_id,
        std::process::id()
    ));
    // Simulate a crash: create a leftover staging temp.
    fs::create_dir_all(&temp).unwrap();
    fs::write(temp.join("partial"), "partial\n").unwrap();

    let migrator = fx.migrator();
    // migrate() should fail with PendingTemp.
    let result = migrator.migrate(fx.request(vec![])).await;
    assert!(matches!(
        result,
        Err(ReadSourceMigrationError::PendingTemp(_))
    ));

    // reconcile() should clean up the temp and succeed.
    let result = migrator.reconcile(fx.request(vec![])).await.unwrap();
    assert!(matches!(result, ReadSourceMigrationResult::Published(_)));
    assert!(dest.exists());
    assert!(!temp.exists(), "temp must be removed after reconcile");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_finalization_record_is_finalized_on_restart() {
    let fx = Fixture::new().await;

    // First: create a pending record by injecting a finalize failure on
    // a clean existing destination.
    let dest = fx.destination();
    make_clean_checkout(&fx.mirror_path, &dest);
    let mut request = fx.request(vec![]);
    request.fail_at = Some(MigrationFailurePoint::FailFinalize);
    let migrator = fx.migrator();
    let _ = migrator.migrate(request).await;

    // Record is pending.
    let repo = ProjectLiveStateMigrationRepository::new(fx.db.clone());
    let (owner, family) = fx.migration_key();
    let record = repo
        .get(fx.make_key(&owner, &family))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.result, "pending");

    // Restart: re-run migrate (no injection). It should find the clean
    // destination and finalize the pending record.
    let result = migrator.migrate(fx.request(vec![])).await.unwrap();
    assert!(matches!(result, ReadSourceMigrationResult::Existing(_)));

    let record = repo
        .get(fx.make_key(&owner, &family))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.result, "succeeded");
}

// ── AC4: Rollback ─────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rollback_preserves_pending_valid_destination_and_records() {
    let fx = Fixture::new().await;
    let dest = fx.destination();

    // Publish a destination.
    let migrator = fx.migrator();
    migrator.migrate(fx.request(vec![])).await.unwrap();
    assert!(dest.exists());

    // Now mark it pending (simulating a state where rollback is needed).
    let repo = ProjectLiveStateMigrationRepository::new(fx.db.clone());
    let (owner, family) = fx.migration_key();
    repo.mark_pending(
        fx.make_key(&owner, &family),
        Some("simulated pending for rollback test"),
    )
    .await
    .unwrap();

    // Rollback.
    migrator
        .rollback(&owner, &fx.target_project_id, &fx.owner_root)
        .await
        .unwrap();

    // A pending finalization can already have published a valid cache.
    // Rollback retains it because no uncertainty may delete valid data.
    assert!(dest.exists(), "pending valid destination must be retained");

    // Record shows rolled_back.
    let record = repo
        .get(fx.make_key(&owner, &family))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.result, "rolled_back");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rollback_retains_finalized_destination() {
    let fx = Fixture::new().await;
    let dest = fx.destination();

    let migrator = fx.migrator();
    migrator.migrate(fx.request(vec![])).await.unwrap();
    assert!(dest.exists());

    // Rollback a finalized (succeeded) migration — destination is valid
    // and retained.
    let (owner, _) = fx.migration_key();
    migrator
        .rollback(&owner, &fx.target_project_id, &fx.owner_root)
        .await
        .unwrap();

    assert!(dest.exists(), "finalized destination must be retained");
    let repo = ProjectLiveStateMigrationRepository::new(fx.db.clone());
    let (owner, family) = fx.migration_key();
    let key = MigrationKey {
        project_id: &owner,
        family: &family,
        release: RELEASE,
    };
    let record = repo.get(key).await.unwrap().unwrap();
    assert_eq!(record.result, "rolled_back");
}

// ── classify unit tests ────────────────────────────────────────────────

#[test]
fn classify_missing() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(
        classify(&tmp.path().join("nope"), "abc"),
        ReadSourcePathState::Missing
    );
}

#[test]
fn classify_symlink() {
    let tmp = tempfile::tempdir().unwrap();
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink("/etc/hostname", &link).unwrap();
    assert_eq!(classify(&link, "abc"), ReadSourcePathState::Symlink);
}

#[test]
fn classify_file() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("file");
    fs::write(&file, "x").unwrap();
    assert_eq!(classify(&file, "abc"), ReadSourcePathState::File);
}

#[test]
fn classify_special_file_fifo() {
    let tmp = tempfile::tempdir().unwrap();
    let fifo = tmp.path().join("fifo");
    // Create a named pipe (fifo) — a platform-supported special file.
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo available on unix");
    assert!(status.success(), "mkfifo should succeed");
    assert_eq!(classify(&fifo, "abc"), ReadSourcePathState::Special);
}

#[test]
fn classify_symlink_inside_directory_tree() {
    // A symlink inside the directory tree is classified as Symlink, not
    // conflated to UnknownEntry.
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    std::os::unix::fs::symlink("/etc/hostname", repo.join("dangling-link")).unwrap();
    assert_eq!(classify(&repo, "abc"), ReadSourcePathState::Symlink);
}

#[test]
fn classify_special_inside_directory_tree() {
    // A special file inside the directory tree is classified as Special.
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    let fifo = repo.join("myfifo");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo available on unix");
    assert!(status.success());
    assert_eq!(classify(&repo, "abc"), ReadSourcePathState::Special);
}

#[test]
fn classify_invalid_git() {
    // A directory that is not a git repository returns InvalidGit.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("not-a-repo");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("file.txt"), "content").unwrap();
    assert_eq!(classify(&dir, "abc"), ReadSourcePathState::InvalidGit);
}

#[test]
fn classify_clean_detached() {
    // A clean detached checkout at the target commit returns Clean.
    let tmp = tempfile::tempdir().unwrap();
    let commit = init_bare_mirror(tmp.path());
    let checkout = tmp.path().join("checkout");
    make_clean_checkout(&tmp.path().join("mirror.git"), &checkout);
    assert_eq!(
        classify(&checkout, &commit),
        ReadSourcePathState::Clean {
            commit: commit.clone()
        }
    );
}

#[test]
fn classify_on_branch() {
    // A checkout on a branch (not detached) returns OnBranch.
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    fs::create_dir_all(&source).unwrap();
    run_git(&source, &["init"]);
    run_git(&source, &["config", "user.email", "t@t.com"]);
    run_git(&source, &["config", "user.name", "T"]);
    fs::write(source.join("file"), "x").unwrap();
    run_git(&source, &["add", "."]);
    run_git(&source, &["commit", "-m", "x"]);
    // On a branch (master/main), not detached.
    assert_eq!(classify(&source, "abc"), ReadSourcePathState::OnBranch);
}

#[test]
fn classify_identity_mismatch() {
    // A clean detached checkout at a different commit returns IdentityMismatch.
    let tmp = tempfile::tempdir().unwrap();
    let _ = init_bare_mirror(tmp.path());
    let checkout = tmp.path().join("checkout");
    make_clean_checkout(&tmp.path().join("mirror.git"), &checkout);
    assert_eq!(
        classify(&checkout, "wrongcommit"),
        ReadSourcePathState::IdentityMismatch {
            commit: git(&checkout, &["rev-parse", "HEAD"]).unwrap(),
        }
    );
}

#[test]
fn classify_staged_dirty_tracked() {
    // Staged-only modifications (`M ` index column) are classified as DirtyTracked.
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    fs::create_dir_all(&source).unwrap();
    run_git(&source, &["init"]);
    run_git(&source, &["config", "user.email", "t@t.com"]);
    run_git(&source, &["config", "user.name", "T"]);
    fs::write(source.join("file"), "original").unwrap();
    run_git(&source, &["add", "."]);
    run_git(&source, &["commit", "-m", "x"]);
    // Stage a modification.
    fs::write(source.join("file"), "modified").unwrap();
    run_git(&source, &["add", "."]);
    // Detach HEAD so it's not OnBranch.
    let head = git(&source, &["rev-parse", "HEAD"]).unwrap();
    run_git(&source, &["checkout", "--detach", &head]);
    assert_eq!(classify(&source, &head), ReadSourcePathState::DirtyTracked);
}

#[test]
fn classify_unstaged_dirty_tracked() {
    // Unstaged-only modifications (` M` worktree column) are classified as DirtyTracked.
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    fs::create_dir_all(&source).unwrap();
    run_git(&source, &["init"]);
    run_git(&source, &["config", "user.email", "t@t.com"]);
    run_git(&source, &["config", "user.name", "T"]);
    fs::write(source.join("file"), "original").unwrap();
    run_git(&source, &["add", "."]);
    run_git(&source, &["commit", "-m", "x"]);
    // Unstaged modification.
    fs::write(source.join("file"), "modified").unwrap();
    // Detach HEAD so it's not OnBranch.
    let head = git(&source, &["rev-parse", "HEAD"]).unwrap();
    run_git(&source, &["checkout", "--detach", &head]);
    assert_eq!(classify(&source, &head), ReadSourcePathState::DirtyTracked);
}

#[test]
fn classify_untracked_in_subdir() {
    // Untracked content in a subdirectory is classified as Untracked.
    let tmp = tempfile::tempdir().unwrap();
    let _ = init_bare_mirror(tmp.path());
    let checkout = tmp.path().join("checkout");
    make_clean_checkout(&tmp.path().join("mirror.git"), &checkout);
    let head = git(&checkout, &["rev-parse", "HEAD"]).unwrap();
    fs::create_dir_all(checkout.join("subdir")).unwrap();
    fs::write(checkout.join("subdir/untracked.txt"), "stuff").unwrap();
    assert_eq!(classify(&checkout, &head), ReadSourcePathState::Untracked);
}

// ── Byte-for-byte preservation helpers ──────────────────────────────────

/// Snapshot every entry in a directory tree, including Git's index and object
/// data, for byte-for-byte comparison before/after a migration attempt.
///
/// This deliberately uses no-follow metadata and never reads a special file:
/// opening a FIFO would block and following a symlink would snapshot the wrong
/// object. For special files the no-follow type, mode, and device identity are
/// part of the snapshot instead of file contents.
fn snapshot_bytes(path: &Path) -> Vec<u8> {
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
