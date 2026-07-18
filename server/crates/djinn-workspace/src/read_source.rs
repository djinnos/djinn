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
        let lock = ProjectLiveStateMigrationLock::try_acquire(&runtime, &request.owner_project_id)?;
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
        let family = format!("read_source:{}", request.target_project_id);
        let repo = ProjectLiveStateMigrationRepository::new(self.db.clone());
        repo.begin(BeginProjectLiveStateMigration {
            project_id: &request.owner_project_id,
            family: &family,
            release: RELEASE,
            source_inventory: &provisional,
            destination: &destination_text,
            pre_hash: None,
            rollback_instruction: ROLLBACK_INSTRUCTION,
        })
        .await?;

        let temp = Self::staging_path(parent, &request.target_project_id);
        if temp.is_symlink() {
            let detail = format!("staging temp is a symlink: {}", temp.display());
            repo.fail(
                MigrationKey {
                    project_id: &request.owner_project_id,
                    family: &family,
                    release: RELEASE,
                },
                &detail,
            )
            .await?;
            return Err(ReadSourceMigrationError::Ambiguous(detail));
        }
        if temp.exists()
            && let Err(source) = fs::remove_dir_all(&temp)
        {
            let detail = format!("failed to remove staging temp {}: {source}", temp.display());
            repo.fail(
                MigrationKey {
                    project_id: &request.owner_project_id,
                    family: &family,
                    release: RELEASE,
                },
                &detail,
            )
            .await?;
            return Err(ReadSourceMigrationError::Io { path: temp, source });
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
        if temp.is_symlink() {
            let detail = format!("staging temp is a symlink: {}", temp.display());
            repo.fail(
                MigrationKey {
                    project_id: owner_project_id,
                    family: &family,
                    release: RELEASE,
                },
                &detail,
            )
            .await?;
            return Err(ReadSourceMigrationError::Ambiguous(detail));
        }
        if temp.exists()
            && let Err(source) = fs::remove_dir_all(&temp)
        {
            let detail = format!("failed to remove staging temp {}: {source}", temp.display());
            repo.fail(
                MigrationKey {
                    project_id: owner_project_id,
                    family: &family,
                    release: RELEASE,
                },
                &detail,
            )
            .await?;
            return Err(ReadSourceMigrationError::Io { path: temp, source });
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
        let lock = ProjectLiveStateMigrationLock::try_acquire(&runtime, &request.owner_project_id)?;
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
                // Retain the provisional `sources` list. `begin` refreshes
                // pending records, so mirror-only failure data would discard
                // caller-discovered legacy inputs.
                let mut inventory = provisional.clone();
                inventory["mirror"] = json!({
                    "path": request.mirror_path,
                    "state": "invalid",
                    "detail": detail,
                });
                inventory["result"] = json!("fail_closed");
                inventory["reason"] = json!("invalid_mirror");
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
        djinn_git::run_git_command_binary_in(
            temp.parent().unwrap_or(destination),
            vec![
                "clone".to_owned(),
                "--local".to_owned(),
                "--shared".to_owned(),
                "--no-checkout".to_owned(),
                request.mirror_path.to_string_lossy().into_owned(),
                temp.to_string_lossy().into_owned(),
            ],
        )
        .map_err(|error| ReadSourceMigrationError::Git(error.to_string()))?;

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
    // Check for symlinks or special files inside the directory tree using
    // no-follow metadata. Each unsafe entry maps to its exact typed state
    // class so fleet reporting can observe the exact disposition.
    if let Some(state) = find_unsafe_entry(path) {
        return state;
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
/// files. Returns the exact typed `ReadSourcePathState` of the first unsafe
/// entry found, or `None` if the tree is clean. Uses no-follow metadata
/// (`symlink_metadata`) throughout.
fn find_unsafe_entry(path: &Path) -> Option<ReadSourcePathState> {
    fn inner(path: &Path) -> Option<ReadSourcePathState> {
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(_) => return Some(ReadSourcePathState::UnknownEntry), // unreadable
        };
        for entry in entries.flatten() {
            let entry_path = entry.path();
            let metadata = match fs::symlink_metadata(&entry_path) {
                Ok(metadata) => metadata,
                Err(_) => return Some(ReadSourcePathState::UnknownEntry),
            };
            if metadata.file_type().is_symlink() {
                return Some(ReadSourcePathState::Symlink);
            }
            if !metadata.is_file() && !metadata.is_dir() {
                return Some(ReadSourcePathState::Special);
            }
            if metadata.is_dir()
                && let Some(state) = inner(&entry_path)
            {
                return Some(state);
            }
        }
        None
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
    // Classification is an inspection boundary. In particular, `git status`
    // normally refreshes and writes `.git/index`; disable optional Git locks so
    // looking at an ambiguous legacy checkout cannot mutate its repository
    // metadata while deciding to fail closed.
    let mut command = vec!["--no-optional-locks".to_owned()];
    command.extend(args.iter().map(|arg| (*arg).to_owned()));
    let output =
        djinn_git::run_git_command_binary_in(path, command).map_err(|error| error.to_string())?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests;
