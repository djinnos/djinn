//! Fail-closed Release N owner-cache read-source migration primitive.
//!
//! The engine owns the complete legacy-input inventory: it enumerates the
//! expected project-local legacy parents, classifies each entry with no-follow
//! metadata, and only ever publishes a clean detached checkout regenerated
//! from the correct bare mirror. No ambiguous, active, DB-uncertain,
//! malformed, conflicting, or injected-failure state may alter either legacy
//! input or a valid destination.
//!
//! Ordering contract (AC2/AC3): every attempt acquires the shared project lock
//! and emits the durable `begin` record **before** any mirror/liveness
//! inspection. Only after the full inventory is classified and proven
//! clean/regenerable does the engine decide whether to accept an existing
//! destination or regenerate one.
//!
//! See `docs/read-source-ownership-and-migration-semantics.md` and the
//! `m0ed` spike for the canonical ownership and legacy-inventory contract.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use djinn_core::live_state_migration::{ProjectLiveStateMigrationLock, atomic_rename};
use djinn_db::{
    BeginProjectLiveStateMigration, Database, MigrationKey, ProjectLiveStateMigrationRepository,
    TaskRunRepository,
};
use serde::Serialize;
use serde_json::{Value, json};
use thiserror::Error;

pub const RELEASE: &str = "N";

/// The kind of legacy input. Used to tag entries in the structured inventory
/// and to enumerate expected parents so the engine can detect unknown
/// sibling/parent entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacyKind {
    /// `<owner root>/.djinn/read-sources/<target_project_id>` — the documented
    /// project-level read source.
    ProjectLocal,
    /// `<workspace>/.djinn-read-sources/<target_project_id>` — the task-local
    /// ephemeral checkout created by the pre-migration agent.
    TaskLocal,
}

/// A classified legacy read source.
///
/// Both shapes are Release N rollback/evidence inputs; neither is ever moved
/// or merged into the canonical destination.
#[derive(Clone, Debug, Serialize)]
pub struct LegacyReadSource {
    pub kind: LegacyKind,
    pub path: PathBuf,
}

/// No-follow path classification. Each named state class from the acceptance
/// criteria has a distinct variant so callers (and fleet reporting) can observe
/// the exact disposition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadSourcePathState {
    /// Path does not exist.
    Missing,
    /// Clean detached checkout at the target commit: no tracked modifications,
    /// no untracked files, no ignored files, and HEAD is detached at `commit`.
    Clean { commit: String },
    /// Tracked files have been modified, staged, or deleted.
    DirtyTracked,
    /// Untracked (but not ignored) files are present.
    Untracked,
    /// Git-ignored files are present.
    Ignored,
    /// HEAD is on a branch (not detached) — ambiguous ownership.
    OnBranch,
    /// The path (or an entry within it) is a symlink.
    Symlink,
    /// The path is a regular file where a directory is expected.
    File,
    /// The path is a special file (socket, fifo, device, etc.).
    Special,
    /// An unknown sibling or parent entry exists that the engine did not
    /// expect and cannot classify.
    UnknownEntry,
    /// Not a valid git repository.
    InvalidGit,
    /// A valid detached git checkout but at the wrong commit.
    IdentityMismatch { commit: String },
}

impl ReadSourcePathState {
    /// `true` for states that are safe to proceed from: absent or provably
    /// clean.
    fn is_clean_or_absent(&self) -> bool {
        matches!(self, Self::Missing | Self::Clean { .. })
    }
}

impl std::fmt::Display for ReadSourcePathState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(f, "missing"),
            Self::Clean { commit } => write!(f, "clean@{commit}"),
            Self::DirtyTracked => write!(f, "dirty_tracked"),
            Self::Untracked => write!(f, "untracked"),
            Self::Ignored => write!(f, "ignored"),
            Self::OnBranch => write!(f, "on_branch"),
            Self::Symlink => write!(f, "symlink"),
            Self::File => write!(f, "file"),
            Self::Special => write!(f, "special"),
            Self::UnknownEntry => write!(f, "unknown_entry"),
            Self::InvalidGit => write!(f, "invalid_git"),
            Self::IdentityMismatch { commit } => write!(f, "identity_mismatch@{commit}"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ReadSourceMigrationRequest {
    pub owner_project_id: String,
    pub target_project_id: String,
    pub owner_root: PathBuf,
    pub mirror_path: PathBuf,
    /// Legacy inputs. The engine also validates that the expected legacy
    /// parents contain no unexpected entries (AC1 "unknown parent entries").
    pub legacy_inputs: Vec<LegacyReadSource>,
    /// Optional fault-injection hooks for tests. Production callers pass `None`.
    pub fail_at: Option<MigrationFailurePoint>,
}

impl ReadSourceMigrationRequest {
    /// Production constructor: no fault injection.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        owner_project_id: String,
        target_project_id: String,
        owner_root: PathBuf,
        mirror_path: PathBuf,
        legacy_inputs: Vec<LegacyReadSource>,
    ) -> Self {
        Self {
            owner_project_id,
            target_project_id,
            owner_root,
            mirror_path,
            legacy_inputs,
            fail_at: None,
        }
    }
}

/// Test-only fault injection points, modeled after the foundation's
/// `AtomicWriteStep`. Each lets a test prove that a specific failure leaves
/// all legacy inputs and any valid destination byte-for-byte intact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MigrationFailurePoint {
    /// Fail the `git clone` step (the clone command itself exits non-zero).
    FailClone,
    /// Fail the `git checkout` step inside the staging clone.
    FailCheckout,
    /// Fail the atomic rename (publish) step.
    FailRename,
    /// Fail the durable `finalize` record after a successful publish.
    FailFinalize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadSourceMigrationResult {
    /// A valid clean detached destination already existed and was left intact.
    Existing(PathBuf),
    /// A clean detached destination was published from the bare mirror.
    Published(PathBuf),
}

impl ReadSourceMigrationResult {
    pub fn path(&self) -> &Path {
        match self {
            Self::Existing(p) | Self::Published(p) => p,
        }
    }
}

#[derive(Debug, Error)]
pub enum ReadSourceMigrationError {
    #[error("active legacy workspace is using a read source: {0}")]
    ActiveWorkspace(String),
    #[error("ambiguous read-source state: {0}")]
    Ambiguous(String),
    #[error("unknown legacy entry under {parent}: {entry}")]
    UnknownEntry { parent: String, entry: String },
    #[error("mirror is not a valid git repository: {0}")]
    InvalidMirror(String),
    #[error("git: {0}")]
    Git(String),
    #[error("migration temp directory already exists; reconcile or rollback required: {0}")]
    PendingTemp(PathBuf),
    #[error("I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Database(#[from] djinn_db::Error),
    #[error(transparent)]
    LiveState(#[from] djinn_core::live_state_migration::LiveStateMigrationError),
    #[error("injected failure at {0:?}")]
    InjectedFailure(MigrationFailurePoint),
}

pub struct ReadSourceMigrator {
    db: Database,
}

impl ReadSourceMigrator {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// The canonical cache destination for `(owner, target)`.
    pub fn destination_for(owner_root: &Path, target_project_id: &str) -> PathBuf {
        owner_root
            .join(".task-runtime/read-sources")
            .join(target_project_id)
    }

    /// Deterministic restart reconciliation for a pending or failed migration.
    ///
    /// The owner lock is acquired before the deterministic staging path is
    /// inspected or removed. The locked helper then performs migration without
    /// recursively acquiring the same non-reentrant lock.
    pub async fn reconcile(
        &self,
        request: ReadSourceMigrationRequest,
    ) -> Result<ReadSourceMigrationResult, ReadSourceMigrationError> {
        let runtime = request.owner_root.join(".task-runtime");
        fs::create_dir_all(&runtime).map_err(|source| ReadSourceMigrationError::Io {
            path: runtime.clone(),
            source,
        })?;
        let lock =
            ProjectLiveStateMigrationLock::try_acquire(&runtime, &request.owner_project_id)?;
        self.reconcile_locked(request, &lock).await
    }

    async fn reconcile_locked(
        &self,
        request: ReadSourceMigrationRequest,
        _lock: &ProjectLiveStateMigrationLock,
    ) -> Result<ReadSourceMigrationResult, ReadSourceMigrationError> {
        let destination = Self::destination_for(&request.owner_root, &request.target_project_id);
        let parent = destination
            .parent()
            .expect("destination always has a read-sources parent");
        // Reconciliation itself is a lifecycle attempt: persist its
        // owner/target-scoped provisional record before touching staging.
        let provisional = provisional_inventory(&request, &destination);
        let destination_text = destination.display().to_string();
        ProjectLiveStateMigrationRepository::new(self.db.clone())
            .begin(BeginProjectLiveStateMigration {
                project_id: &request.owner_project_id,
                family: &format!("read_source:{}", request.target_project_id),
                release: RELEASE,
                source_inventory: &provisional,
                destination: &destination_text,
                pre_hash: None,
                rollback_instruction: ROLLBACK_INSTRUCTION,
            })
            .await?;

        let temp = Self::staging_path(parent, &request.target_project_id);
        if temp.is_symlink() {
            return Err(ReadSourceMigrationError::Ambiguous(format!(
                "staging temp is a symlink: {}",
                temp.display()
            )));
        }
        if temp.exists() {
            fs::remove_dir_all(&temp).map_err(|source| ReadSourceMigrationError::Io {
                path: temp.clone(),
                source,
            })?;
        }
        self.migrate_locked(request, _lock).await
    }

    /// Record a rollback while retaining all legacy inputs and destination.
    /// Rollback has no mirror identity with which to prove a destination safe
    /// to delete, so it only removes a private staging tree under the lock.
    pub async fn rollback(
        &self,
        owner_project_id: &str,
        target_project_id: &str,
        owner_root: &Path,
    ) -> Result<(), ReadSourceMigrationError> {
        let runtime = owner_root.join(".task-runtime");
        fs::create_dir_all(&runtime).map_err(|source| ReadSourceMigrationError::Io {
            path: runtime.clone(),
            source,
        })?;
        let lock = ProjectLiveStateMigrationLock::try_acquire(&runtime, owner_project_id)?;
        self.rollback_locked(owner_project_id, target_project_id, owner_root, &lock)
            .await
    }

    async fn rollback_locked(
        &self,
        owner_project_id: &str,
        target_project_id: &str,
        owner_root: &Path,
        _lock: &ProjectLiveStateMigrationLock,
    ) -> Result<(), ReadSourceMigrationError> {
        let family = format!("read_source:{target_project_id}");
        let destination = Self::destination_for(owner_root, target_project_id);
        let parent = destination
            .parent()
            .expect("destination always has a read-sources parent");
        let repo = ProjectLiveStateMigrationRepository::new(self.db.clone());
        let provisional = json!({
            "owner_project_id": owner_project_id,
            "target_project_id": target_project_id,
            "sources": [],
            "destination": { "path": destination, "state": "uninspected" },
            "attempt": "rollback_before_inspection"
        });
        let destination_text = destination.display().to_string();
        repo.begin(BeginProjectLiveStateMigration {
            project_id: owner_project_id,
            family: &family,
            release: RELEASE,
            source_inventory: &provisional,
            destination: &destination_text,
            pre_hash: None,
            rollback_instruction: ROLLBACK_INSTRUCTION,
        })
        .await?;
        let temp = Self::staging_path(parent, target_project_id);
        if temp.exists() && !temp.is_symlink() {
            fs::remove_dir_all(&temp)
                .map_err(|source| ReadSourceMigrationError::Io { path: temp, source })?;
        }
        repo.rollback(
            MigrationKey {
                project_id: owner_project_id,
                family: &family,
                release: RELEASE,
            },
            Some("operator rollback: retain all legacy inputs and destination"),
        )
        .await?;
        Ok(())
    }

    fn staging_path(parent: &Path, target_project_id: &str) -> PathBuf {
        parent.join(format!(
            ".{}.read-source-migration.{}",
            target_project_id,
            std::process::id()
        ))
    }

    pub async fn migrate(
        &self,
        request: ReadSourceMigrationRequest,
    ) -> Result<ReadSourceMigrationResult, ReadSourceMigrationError> {
        let runtime = request.owner_root.join(".task-runtime");
        fs::create_dir_all(&runtime).map_err(|source| ReadSourceMigrationError::Io {
            path: runtime.clone(),
            source,
        })?;
        let lock =
            ProjectLiveStateMigrationLock::try_acquire(&runtime, &request.owner_project_id)?;
        self.migrate_locked(request, &lock).await
    }

    async fn migrate_locked(
        &self,
        request: ReadSourceMigrationRequest,
        _lock: &ProjectLiveStateMigrationLock,
    ) -> Result<ReadSourceMigrationResult, ReadSourceMigrationError> {
        let destination = Self::destination_for(&request.owner_root, &request.target_project_id);
        let family = format!("read_source:{}", request.target_project_id);

        let repo = ProjectLiveStateMigrationRepository::new(self.db.clone());
        let key = MigrationKey {
            project_id: &request.owner_project_id,
            family: &family,
            release: RELEASE,
        };

        // An attempt is durable before inspecting the mirror or asking the DB.
        let provisional = provisional_inventory(&request, &destination);
        let dest_text = destination.display().to_string();
        repo.begin(BeginProjectLiveStateMigration {
            project_id: &request.owner_project_id,
            family: &family,
            release: RELEASE,
            source_inventory: &provisional,
            destination: &dest_text,
            pre_hash: None,
            rollback_instruction: ROLLBACK_INSTRUCTION,
        })
        .await?;

        // ── Mirror inspection ─────────────────────────────────────────────
        // We need the target commit for the inventory, but we must emit the
        // `begin` record even if the mirror is invalid, so the durable record
        // captures the failure (AC3).
        let mirror_state = classify_mirror(&request.mirror_path);
        let target_commit = match &mirror_state {
            MirrorState::Valid(commit) => commit.clone(),
            MirrorState::Invalid(detail) => {
                let inventory = json!({
                    "owner_project_id": request.owner_project_id,
                    "target_project_id": request.target_project_id,
                    "mirror": {"path": request.mirror_path, "state": "invalid", "detail": detail},
                    "result": "fail_closed",
                    "reason": "invalid_mirror",
                });
                self.begin_and_fail(
                    &request.owner_project_id,
                    &family,
                    &destination,
                    &inventory,
                    &format!("invalid mirror: {detail}"),
                )
                .await?;
                return Err(ReadSourceMigrationError::InvalidMirror(detail.clone()));
            }
        };

        // ── Liveness query (AC2: DB-uncertain fails closed) ───────────────
        // Errors propagate; the caller cannot mistake DB uncertainty for idle.
        let task_runs = TaskRunRepository::new(self.db.clone());
        let live = match task_runs
            .live_workspace_paths_for_project(&request.owner_project_id)
            .await
        {
            Ok(live) => live,
            Err(error) => {
                let detail = format!("liveness query uncertain: {error}");
                let _ = repo.fail(key, &detail).await;
                return Err(ReadSourceMigrationError::Database(error));
            }
        };
        let workspace_paths = match task_runs
            .workspace_paths_for_project(&request.owner_project_id)
            .await
        {
            Ok(paths) => paths,
            Err(error) => {
                let detail = format!("workspace inventory query uncertain: {error}");
                let _ = repo.fail(key, &detail).await;
                return Err(ReadSourceMigrationError::Database(error));
            }
        };

        // ── Classify legacy inputs ────────────────────────────────────────
        let legacy_inputs = complete_legacy_inputs(&request, &workspace_paths);
        let legacy_states: Vec<(LegacyKind, PathBuf, ReadSourcePathState)> = legacy_inputs
            .iter()
            .map(|input| {
                let state = classify(&input.path, &target_commit);
                (input.kind, input.path.clone(), state)
            })
            .collect();

        // ── Active workspace check (AC1/AC2) ──────────────────────────────
        // If any legacy input is under a live workspace path, fail closed.
        if let Some(path) = legacy_states.iter().find_map(|(_, path, _)| {
            live.iter()
                .find(|workspace| path.starts_with(workspace))
                .map(|_| path.clone())
        }) {
            let active_path = path.display().to_string();
            let inventory = build_inventory(
                &request,
                &target_commit,
                &legacy_states,
                &destination,
                Some(format!("active workspace: {active_path}")),
            );
            self.begin_and_fail(
                &request.owner_project_id,
                &family,
                &destination,
                &inventory,
                &ReadSourceMigrationError::ActiveWorkspace(active_path.clone()).to_string(),
            )
            .await?;
            return Err(ReadSourceMigrationError::ActiveWorkspace(active_path));
        }

        // ── Unknown sibling / parent entry detection (AC1) ────────────────
        if let Err(error) =
            Self::check_unknown_siblings(&request.owner_root, &request.target_project_id)
        {
            let inventory = build_inventory(
                &request,
                &target_commit,
                &legacy_states,
                &destination,
                Some(error.to_string()),
            );
            self.begin_and_fail(
                &request.owner_project_id,
                &family,
                &destination,
                &inventory,
                &error.to_string(),
            )
            .await?;
            return Err(error);
        }

        // ── Classify destination ──────────────────────────────────────────
        let destination_state = classify(&destination, &target_commit);

        // ── Emit durable begin record (AC3) ───────────────────────────────
        let inventory =
            build_inventory(&request, &target_commit, &legacy_states, &destination, None);
        let dest_text = destination.display().to_string();
        repo.begin(BeginProjectLiveStateMigration {
            project_id: &request.owner_project_id,
            family: &family,
            release: RELEASE,
            source_inventory: &inventory,
            destination: &dest_text,
            pre_hash: None,
            rollback_instruction: ROLLBACK_INSTRUCTION,
        })
        .await?;

        // A valid detached source at another commit is an identity mismatch on
        // its own, but two such sources are a dedicated conflict class.
        let observed_commits: Vec<&String> = legacy_states
            .iter()
            .filter_map(|(_, _, state)| match state {
                ReadSourcePathState::Clean { commit }
                | ReadSourcePathState::IdentityMismatch { commit } => Some(commit),
                _ => None,
            })
            .collect();
        if observed_commits.windows(2).any(|pair| pair[0] != pair[1]) {
            let detail = "differing dual legacy inputs";
            let _ = repo.fail(key, detail).await;
            return Err(ReadSourceMigrationError::Ambiguous(detail.into()));
        }

        // ── AC2: inspect legacy inputs BEFORE accepting destination ───────
        // Every legacy input must be Missing or Clean. Any other state
        // (dirty, untracked, ignored, symlink, special, unknown, etc.) fails
        // closed.
        for (kind, path, state) in &legacy_states {
            if !state.is_clean_or_absent() {
                let detail = format!("legacy input {kind:?} at {} is {state}", path.display());
                let _ = repo.fail(key, &detail).await;
                return Err(ReadSourceMigrationError::Ambiguous(detail));
            }
        }

        // ── Destination decision ──────────────────────────────────────────
        // Only now, after all legacy inputs are proven clean/absent, do we
        // decide on the destination. A clean destination at the target commit
        // is accepted. Missing is regenerated. Anything else fails closed.
        match destination_state {
            ReadSourcePathState::Clean { .. } => {
                if request.fail_at == Some(MigrationFailurePoint::FailFinalize) {
                    // Even on finalize injection, the destination is valid and
                    // must be preserved. The record stays pending.
                    return Err(ReadSourceMigrationError::InjectedFailure(
                        MigrationFailurePoint::FailFinalize,
                    ));
                }
                repo.finalize(
                    key,
                    Some(&target_commit),
                    Some("verified existing detached owner cache"),
                )
                .await?;
                return Ok(ReadSourceMigrationResult::Existing(destination.clone()));
            }
            ReadSourcePathState::Missing => {}
            other => {
                let detail = format!("destination is {other}, not clean or missing");
                let _ = repo.fail(key, &detail).await;
                return Err(ReadSourceMigrationError::Ambiguous(detail));
            }
        }

        // ── Regenerate destination from the bare mirror ───────────────────
        let parent = destination
            .parent()
            .expect("destination always has a read-sources parent");
        fs::create_dir_all(parent).map_err(|source| ReadSourceMigrationError::Io {
            path: parent.to_path_buf(),
            source,
        })?;

        let temp = Self::staging_path(parent, &request.target_project_id);

        // If a stale temp exists from a prior crash, this is an ambiguous
        // state requiring explicit reconciliation. Do NOT silently remove it
        // in migrate(); use reconcile() for that.
        if temp.exists() || temp.is_symlink() {
            let detail = format!("migration temp already exists: {}", temp.display());
            let _ = repo.fail(key, &detail).await;
            return Err(ReadSourceMigrationError::PendingTemp(temp));
        }

        let result = self.clone_and_publish(&request, &temp, &destination, &target_commit);

        match result {
            Ok(()) => {
                if request.fail_at == Some(MigrationFailurePoint::FailFinalize) {
                    // Publication stays pending for restart reconciliation; the
                    // verified destination is never removed.
                    return Err(ReadSourceMigrationError::InjectedFailure(
                        MigrationFailurePoint::FailFinalize,
                    ));
                }
                // Finalize. If finalize fails (e.g. injected), the destination
                // has been published but the record stays pending —
                // reconciliation will finalize it.
                if let Err(error) = repo
                    .finalize(
                        key,
                        Some(&target_commit),
                        Some("published detached owner cache from bare mirror"),
                    )
                    .await
                {
                    // Finalization failed; the destination is valid but the
                    // record is pending. Return the DB error so the caller
                    // knows to reconcile, but the destination is intact.
                    return Err(ReadSourceMigrationError::Database(error));
                }
                Ok(ReadSourceMigrationResult::Published(destination.clone()))
            }
            Err(error) => {
                // Ensure staging temp is removed on any clone/checkout/rename
                // failure so a restart does not get stuck (AC4).
                if temp.exists() && !temp.is_symlink() {
                    let _ = fs::remove_dir_all(&temp);
                }
                let _ = repo.fail(key, &error.to_string()).await;
                Err(error)
            }
        }
    }

    /// Enumerate the expected project-local legacy parent directory and check
    /// that it contains no entries other than the expected target child.
    /// Unknown siblings must be reported and must prevent migration (AC1:
    /// unknown parent entries).
    fn check_unknown_siblings(
        owner_root: &Path,
        target: &str,
    ) -> Result<(), ReadSourceMigrationError> {
        let parent = owner_root.join(".djinn/read-sources");
        let expected_child = parent.join(target);
        let entries = match fs::read_dir(&parent) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => {
                return Err(ReadSourceMigrationError::Ambiguous(format!(
                    "cannot read legacy parent {}",
                    parent.display()
                )));
            }
        };
        for entry in entries.flatten() {
            let entry_path = entry.path();
            if entry_path == expected_child {
                continue;
            }
            return Err(ReadSourceMigrationError::UnknownEntry {
                parent: parent.display().to_string(),
                entry: entry_path.display().to_string(),
            });
        }
        Ok(())
    }

    /// Clone from the bare mirror into a staging directory, checkout detached
    /// HEAD, verify, and atomically publish. On any failure the staging
    /// directory is cleaned up by the caller.
    fn clone_and_publish(
        &self,
        request: &ReadSourceMigrationRequest,
        temp: &Path,
        destination: &Path,
        target_commit: &str,
    ) -> Result<(), ReadSourceMigrationError> {
        // ── Clone step ────────────────────────────────────────────────────
        if request.fail_at == Some(MigrationFailurePoint::FailClone) {
            // Simulate clone failure: create the temp dir (as git would) then
            // fail, so the test proves the temp is cleaned up.
            fs::create_dir_all(temp).map_err(|source| ReadSourceMigrationError::Io {
                path: temp.to_path_buf(),
                source,
            })?;
            return Err(ReadSourceMigrationError::InjectedFailure(
                MigrationFailurePoint::FailClone,
            ));
        }
        let output = Command::new("git")
            .args(["clone", "--local", "--shared", "--no-checkout"])
            .arg(&request.mirror_path)
            .arg(temp)
            .output()
            .map_err(|source| ReadSourceMigrationError::Io {
                path: temp.to_path_buf(),
                source,
            })?;
        if !output.status.success() {
            return Err(ReadSourceMigrationError::Git(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }

        // ── Checkout step ─────────────────────────────────────────────────
        if request.fail_at == Some(MigrationFailurePoint::FailCheckout) {
            return Err(ReadSourceMigrationError::InjectedFailure(
                MigrationFailurePoint::FailCheckout,
            ));
        }
        if let Err(error) = git(temp, &["checkout", "--detach", "HEAD"]) {
            return Err(ReadSourceMigrationError::Git(error));
        }

        // ── Verify the regenerated destination is clean at target ─────────
        if !matches!(
            classify(temp, target_commit),
            ReadSourcePathState::Clean { .. }
        ) {
            return Err(ReadSourceMigrationError::Ambiguous(
                "regenerated destination is not clean".into(),
            ));
        }

        // ── Rename step ───────────────────────────────────────────────────
        if request.fail_at == Some(MigrationFailurePoint::FailRename) {
            return Err(ReadSourceMigrationError::InjectedFailure(
                MigrationFailurePoint::FailRename,
            ));
        }
        atomic_rename(temp, destination)?;
        Ok(())
    }

    /// Helper: begin a durable record and immediately fail it. Used for
    /// pre-decision failures (invalid mirror, active workspace, unknown
    /// entry) that still must be recorded per AC3. Called while the lock is
    /// held.
    async fn begin_and_fail(
        &self,
        owner_project_id: &str,
        family: &str,
        destination: &Path,
        inventory: &Value,
        detail: &str,
    ) -> Result<(), ReadSourceMigrationError> {
        let repo = ProjectLiveStateMigrationRepository::new(self.db.clone());
        let dest_text = destination.display().to_string();
        repo.begin(BeginProjectLiveStateMigration {
            project_id: owner_project_id,
            family,
            release: RELEASE,
            source_inventory: inventory,
            destination: &dest_text,
            pre_hash: None,
            rollback_instruction: ROLLBACK_INSTRUCTION,
        })
        .await?;
        let key = MigrationKey {
            project_id: owner_project_id,
            family,
            release: RELEASE,
        };
        repo.fail(key, detail).await?;
        Ok(())
    }
}

const ROLLBACK_INSTRUCTION: &str = "Release N rollback: retain all legacy inputs; remove only a \
    verified detached owner cache under the owner migration lock.";

// ── Mirror classification ─────────────────────────────────────────────────

enum MirrorState {
    Valid(String),
    Invalid(String),
}

fn classify_mirror(path: &Path) -> MirrorState {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return MirrorState::Invalid(format!("mirror is a symlink: {}", path.display()));
            }
            if !metadata.is_dir() {
                return MirrorState::Invalid(format!(
                    "mirror is not a directory: {}",
                    path.display()
                ));
            }
        }
        Err(error) => {
            return MirrorState::Invalid(format!("mirror not accessible: {error}"));
        }
    }
    match git(path, &["rev-parse", "HEAD"]) {
        Ok(commit) => MirrorState::Valid(commit),
        Err(detail) => MirrorState::Invalid(format!("mirror rev-parse failed: {detail}")),
    }
}

// ── Path classification ───────────────────────────────────────────────────

/// Classify a path using no-follow (`symlink_metadata`) inspection before any
/// content/git inspection. Returns the exact named state class.
fn classify(path: &Path, target: &str) -> ReadSourcePathState {
    let metadata = match fs::symlink_metadata(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ReadSourcePathState::Missing;
        }
        Err(_) => return ReadSourcePathState::UnknownEntry,
    };
    if metadata.file_type().is_symlink() {
        return ReadSourcePathState::Symlink;
    }
    if !metadata.is_dir() {
        return if metadata.is_file() {
            ReadSourcePathState::File
        } else {
            ReadSourcePathState::Special
        };
    }
    // Check for symlinks or special files inside the directory tree.
    if has_unsafe_entry(path) {
        return ReadSourcePathState::UnknownEntry;
    }
    let commit = match git(path, &["rev-parse", "HEAD"]) {
        Ok(value) => value,
        Err(_) => return ReadSourcePathState::InvalidGit,
    };
    // Check if HEAD is detached (symbolic-ref fails on detached HEAD).
    let on_branch = git(path, &["symbolic-ref", "-q", "HEAD"]).is_ok();
    if on_branch {
        return ReadSourcePathState::OnBranch;
    }
    // Distinguish tracked dirt, untracked, and ignored.
    let porcelain = git(path, &["status", "--porcelain"]).unwrap_or_default();
    let ignored = git(path, &["status", "--porcelain", "--ignored"]).unwrap_or_default();
    let has_dirty_tracked = porcelain.lines().any(|line| {
        let bytes = line.as_bytes();
        // Porcelain has index and worktree columns. ` M` is an ordinary
        // unstaged tracked edit and must be rejected just like `M `.
        !line.starts_with("??")
            && [bytes.first(), bytes.get(1)]
                .into_iter()
                .flatten()
                .any(|flag| matches!(*flag, b'M' | b'T' | b'R' | b'C' | b'D' | b'A' | b'U'))
    });
    let has_untracked = porcelain.lines().any(|line| line.starts_with("??"));
    let has_ignored = ignored.lines().any(|line| line.starts_with("!!"));
    if has_dirty_tracked {
        return ReadSourcePathState::DirtyTracked;
    }
    if has_untracked {
        return ReadSourcePathState::Untracked;
    }
    if has_ignored {
        return ReadSourcePathState::Ignored;
    }
    if commit != target {
        return ReadSourcePathState::IdentityMismatch { commit };
    }
    ReadSourcePathState::Clean { commit }
}

/// Recursively check that a directory tree contains no symlinks or special
/// files. This is a no-follow safety check.
fn has_unsafe_entry(path: &Path) -> bool {
    fn inner(path: &Path) -> bool {
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(_) => return true, // unreadable → unsafe
        };
        for entry in entries.flatten() {
            let entry_path = entry.path();
            let metadata = match fs::symlink_metadata(&entry_path) {
                Ok(metadata) => metadata,
                Err(_) => return true,
            };
            if metadata.file_type().is_symlink() {
                return true;
            }
            if !metadata.is_file() && !metadata.is_dir() {
                return true;
            }
            if metadata.is_dir() && inner(&entry_path) {
                return true;
            }
        }
        false
    }
    inner(path)
}

/// Build the conservative record persisted before any mirror or DB inspection.
fn provisional_inventory(request: &ReadSourceMigrationRequest, destination: &Path) -> Value {
    let sources: Vec<Value> = request
        .legacy_inputs
        .iter()
        .map(|source| json!({ "kind": source.kind, "path": source.path, "state": "uninspected" }))
        .collect();
    json!({
        "owner_project_id": request.owner_project_id,
        "target_project_id": request.target_project_id,
        "sources": sources,
        "destination": { "path": destination, "state": "uninspected" },
        "attempt": "begin_before_inspection"
    })
}

/// Caller-provided entries supplement, but cannot omit, the expected owner
/// path and task-local paths beneath every recorded owner workspace.
fn complete_legacy_inputs(
    request: &ReadSourceMigrationRequest,
    workspace_paths: &[String],
) -> Vec<LegacyReadSource> {
    let mut inputs = request.legacy_inputs.clone();
    inputs.push(LegacyReadSource {
        kind: LegacyKind::ProjectLocal,
        path: request
            .owner_root
            .join(".djinn/read-sources")
            .join(&request.target_project_id),
    });
    for workspace in workspace_paths {
        inputs.push(LegacyReadSource {
            kind: LegacyKind::TaskLocal,
            path: Path::new(workspace)
                .join(".djinn-read-sources")
                .join(&request.target_project_id),
        });
    }
    inputs.sort_by(|a, b| a.path.cmp(&b.path));
    inputs.dedup_by(|a, b| a.path == b.path);
    inputs
}

/// Build the structured multi-source inventory for the durable record.
fn build_inventory(
    request: &ReadSourceMigrationRequest,
    target_commit: &str,
    legacy_states: &[(LegacyKind, PathBuf, ReadSourcePathState)],
    destination: &Path,
    failure_reason: Option<String>,
) -> Value {
    let sources: Vec<Value> = legacy_states
        .iter()
        .map(|(kind, path, state)| {
            json!({
                "kind": kind,
                "owner_project_id": request.owner_project_id,
                "target_project_id": request.target_project_id,
                "path": path,
                "state": state,
            })
        })
        .collect();
    let dest_state = classify(destination, target_commit);
    let mut inventory = json!({
        "owner_project_id": request.owner_project_id,
        "target_project_id": request.target_project_id,
        "target_commit": target_commit,
        "sources": sources,
        "destination": {
            "path": destination,
            "state": dest_state,
        },
    });
    if let Some(reason) = failure_reason {
        inventory["failure_reason"] = json!(reason);
    }
    inventory
}

fn git(path: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .current_dir(path)
        .args(args)
        .output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_db::Database;

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
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("git command");
        if !output.status.success() {
            panic!(
                "git {} failed in {}: {}",
                args.join(" "),
                dir.display(),
                String::from_utf8_lossy(&output.stderr)
            );
        }
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
            db.ensure_initialized().await.unwrap();
            sqlx::query(
                "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, 'owner', 'test', 'owner')",
            )
            .bind("owner-proj-001")
            .execute(db.pool())
            .await
            .unwrap();
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

        let migrator = fx.migrator();
        let result = migrator
            .migrate(fx.request(vec![fx.legacy_input(LegacyKind::ProjectLocal)]))
            .await;
        assert!(result.is_err(), "must fail closed with dirty legacy input");
        // Destination preserved byte-for-byte.
        assert_eq!(
            classify(&dest, &fx.target_commit),
            ReadSourcePathState::Clean {
                commit: fx.target_commit.clone()
            }
        );
        // Legacy input preserved (dirty).
        assert_eq!(
            classify(&project_path, &fx.target_commit),
            ReadSourcePathState::DirtyTracked
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
        let mut request = fx.request(vec![]);
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
        let task_id = uuid::Uuid::now_v7().to_string();
        let run_id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design, issue_type, priority, owner, status, continuation_count, labels, acceptance_criteria, memory_refs) VALUES ($1, $2, 'tsk', $3, 'T', '', '', 'task', 0, '', 'open', 0, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb)",
        )
        .bind(&task_id)
        .bind("owner-proj-001")
        .bind(uuid::Uuid::now_v7().to_string())
        .execute(fx.db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_runs (id, project_id, task_id, trigger_type, status, workspace_path) VALUES ($1, $2, $3, 'new_task', 'running', $4)",
        )
        .bind(&run_id)
        .bind("owner-proj-001")
        .bind(&task_id)
        .bind(&workspace)
        .execute(fx.db.pool())
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
        sqlx::query("DROP TABLE task_runs")
            .execute(fx.db.pool())
            .await
            .unwrap();

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

        // Owner two — different root.
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
        let lock =
            ProjectLiveStateMigrationLock::try_acquire(&runtime, "owner-proj-001").unwrap();
        let migrator = fx.migrator();
        let (reconcile, rollback) = tokio::join!(
            migrator.reconcile(fx.request(vec![])),
            migrator.rollback("owner-proj-001", &fx.target_project_id, &fx.owner_root),
        );
        assert!(matches!(reconcile, Err(ReadSourceMigrationError::LiveState(_))));
        assert!(matches!(rollback, Err(ReadSourceMigrationError::LiveState(_))));
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
}
