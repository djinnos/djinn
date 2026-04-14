//! Linux-only ADR-057 mounted-memory enablement and guardrails.
//!
//! This module wires the transport-neutral [`crate::memory_fs::MemoryFilesystemCore`] into an
//! initial FUSE adapter behind the `memory-mount` cargo feature and typed server settings:
//!
//! - `memory_mount_enabled: true`
//! - `memory_mount_path: "/absolute/mount/path"`
//!
//! The mount is disabled by default. When enabled on Linux with the cargo feature present, Djinn
//! mounts the single registered project's memory note tree at `memory_mount_path` using FUSE.
//! Runtime note operations resolve through the active task/session worktree context when one can be
//! determined. Otherwise the mount falls back to the canonical `main` view.
//!
//! Filesystem-first usage guardrails for this slice:
//! - `.djinn/memory/` represents the current session-selected view, not a branch-directory UX
//! - when one active task with a non-canonical worktree is available for the mounted project, the
//!   mount reflects that task/worktree view
//! - when task/session/worktree resolution fails, or the active session is still on the canonical
//!   project root, the mount intentionally falls back to the canonical `main` view
//! - callers that need an explicit canonical read should not infer it from the mount path alone
//! - no `@main`, `@task_*`, or other explicit branch directory affordances are supported here
//!
//! Operational safety for this wave:
//! - startup rejects invalid configuration before the HTTP server begins serving
//! - the mountpoint must already exist, be absolute, and be empty
//! - only Linux + `--features memory-mount` is supported in this slice
//! - exactly one registered project is supported; branch-aware and multi-project mounting remain
//!   out of scope for later ADR-057 waves

use std::path::PathBuf;

#[cfg(any(test, all(target_os = "linux", feature = "memory-mount")))]
use std::collections::HashMap;
#[cfg(all(target_os = "linux", feature = "memory-mount"))]
use std::ffi::OsStr;
#[cfg(all(target_os = "linux", feature = "memory-mount"))]
use std::hash::{Hash, Hasher};
#[cfg(all(target_os = "linux", feature = "memory-mount"))]
use std::path::Path;
#[cfg(all(target_os = "linux", feature = "memory-mount"))]
use std::sync::{Arc, Mutex};
#[cfg(all(target_os = "linux", feature = "memory-mount"))]
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use djinn_core::models::DjinnSettings;
#[cfg(all(target_os = "linux", feature = "memory-mount"))]
use djinn_db::NoteRepository;
use djinn_db::ProjectRepository;

use crate::events::EventBus;
#[cfg(all(target_os = "linux", feature = "memory-mount"))]
use crate::memory_fs::{
    MemoryEntryKind, MemoryEntryMetadata, MemoryFilesystemCore, MemoryViewSelection,
};

#[cfg(all(target_os = "linux", feature = "memory-mount"))]
const WRITE_DEBOUNCE_WINDOW: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryMountConfig {
    pub enabled: bool,
    pub mount_path: Option<PathBuf>,
}

impl MemoryMountConfig {
    pub fn from_settings(settings: &DjinnSettings) -> Self {
        Self {
            enabled: settings.memory_mount_enabled.unwrap_or(false),
            mount_path: settings.memory_mount_path.as_ref().map(PathBuf::from),
        }
    }
}

#[derive(Debug)]
pub struct MountedMemoryFilesystem {
    runtime_status: std::sync::Arc<tokio::sync::Mutex<MemoryMountRuntimeStatus>>,
    #[cfg(all(target_os = "linux", feature = "memory-mount"))]
    session: Option<fuser::BackgroundSession>,
}

impl MountedMemoryFilesystem {
    pub fn disabled() -> Self {
        Self {
            runtime_status: std::sync::Arc::new(tokio::sync::Mutex::new(
                MemoryMountRuntimeStatus::disabled(),
            )),
            #[cfg(all(target_os = "linux", feature = "memory-mount"))]
            session: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_status(status: MemoryMountRuntimeStatus) -> Self {
        Self {
            runtime_status: std::sync::Arc::new(tokio::sync::Mutex::new(status)),
            #[cfg(all(target_os = "linux", feature = "memory-mount"))]
            session: None,
        }
    }

    #[cfg(all(target_os = "linux", feature = "memory-mount"))]
    fn from_session(
        session: fuser::BackgroundSession,
        runtime_status: std::sync::Arc<tokio::sync::Mutex<MemoryMountRuntimeStatus>>,
    ) -> Self {
        Self {
            runtime_status,
            session: Some(session),
        }
    }

    pub(crate) async fn status_snapshot(&self) -> MemoryMountRuntimeStatus {
        self.runtime_status.lock().await.clone()
    }

    #[cfg(all(target_os = "linux", feature = "memory-mount"))]
    pub fn is_active(&self) -> bool {
        self.session.is_some()
    }

    #[cfg(not(all(target_os = "linux", feature = "memory-mount")))]
    pub fn is_active(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemoryMountRuntimeStatus {
    pub(crate) lifecycle: crate::server::MemoryMountLifecycleState,
    pub(crate) configured: bool,
    pub(crate) mount_path: Option<PathBuf>,
    pub(crate) project_id: Option<String>,
    pub(crate) detail: Option<String>,
    pub(crate) pending_writes: usize,
    pub(crate) last_error: Option<String>,
}

#[cfg_attr(
    not(all(target_os = "linux", feature = "memory-mount")),
    allow(dead_code)
)]
impl MemoryMountRuntimeStatus {
    pub(crate) fn disabled() -> Self {
        Self {
            lifecycle: crate::server::MemoryMountLifecycleState::Disabled,
            configured: false,
            mount_path: None,
            project_id: None,
            detail: None,
            pending_writes: 0,
            last_error: None,
        }
    }

    pub(crate) fn configured(mount_path: PathBuf, project_id: String) -> Self {
        Self {
            lifecycle: crate::server::MemoryMountLifecycleState::Configured,
            configured: true,
            mount_path: Some(mount_path),
            project_id: Some(project_id),
            detail: Some("mount validated but not yet attached".to_string()),
            pending_writes: 0,
            last_error: None,
        }
    }

    fn mark_mounted(&mut self) {
        self.lifecycle = crate::server::MemoryMountLifecycleState::Mounted;
        self.detail = None;
    }

    fn mark_degraded(&mut self, detail: impl Into<String>) {
        let detail = detail.into();
        self.lifecycle = crate::server::MemoryMountLifecycleState::Degraded;
        self.detail = Some(detail.clone());
        self.last_error = Some(detail);
    }

    fn set_pending_writes(&mut self, pending_writes: usize) {
        self.pending_writes = pending_writes;
    }
}

#[cfg(not(target_os = "linux"))]
fn ensure_mount_runtime_support() -> Result<()> {
    bail!("memory mount is only supported on Linux in ADR-057 wave 1")
}

#[cfg(all(target_os = "linux", not(feature = "memory-mount")))]
fn ensure_mount_runtime_support() -> Result<()> {
    bail!(
        "memory mount is enabled in settings but djinn-server was built without the `memory-mount` feature"
    )
}

#[cfg(all(target_os = "linux", feature = "memory-mount"))]
fn ensure_mount_runtime_support() -> Result<()> {
    Ok(())
}

pub async fn validate_mount_config(
    settings: &DjinnSettings,
    db: djinn_db::Database,
    events: EventBus,
) -> Result<Option<ResolvedMemoryMount>> {
    let config = MemoryMountConfig::from_settings(settings);
    if !config.enabled {
        return Ok(None);
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (db, events);
        ensure_mount_runtime_support()?;
        unreachable!("non-Linux support check should always fail")
    }

    #[cfg(target_os = "linux")]
    {
        ensure_mount_runtime_support()?;

        let mount_path = config.mount_path.ok_or_else(|| {
            anyhow!("memory_mount_path must be set when memory_mount_enabled is true")
        })?;

        if !mount_path.is_absolute() {
            bail!(
                "memory_mount_path must be an absolute path, got {}",
                mount_path.display()
            );
        }

        let metadata = std::fs::metadata(&mount_path).with_context(|| {
            format!(
                "memory mount path does not exist or is inaccessible: {}",
                mount_path.display()
            )
        })?;
        if !metadata.is_dir() {
            bail!(
                "memory mount path must be a directory: {}",
                mount_path.display()
            );
        }
        if std::fs::read_dir(&mount_path)
            .with_context(|| format!("failed to inspect mount path: {}", mount_path.display()))?
            .next()
            .is_some()
        {
            bail!(
                "memory mount path must be empty before mounting: {}",
                mount_path.display()
            );
        }

        let project_repo = ProjectRepository::new(db, events);
        let projects = project_repo
            .list()
            .await
            .context("failed to list projects for memory mount startup")?;

        let [project] = projects.as_slice() else {
            bail!(
                "memory mount currently requires exactly one registered project; found {}",
                projects.len()
            );
        };

        Ok(Some(ResolvedMemoryMount {
            mount_path,
            project_id: project.id.clone(),
            project_path: PathBuf::from(&project.path),
        }))
    }
}

pub async fn start_memory_mount(
    settings: &DjinnSettings,
    state: crate::server::AppState,
) -> Result<Option<MountedMemoryFilesystem>> {
    let Some(resolved) =
        validate_mount_config(settings, state.db().clone(), state.event_bus()).await?
    else {
        return Ok(None);
    };

    #[cfg(all(target_os = "linux", feature = "memory-mount"))]
    {
        let repo = NoteRepository::new(state.db().clone(), state.event_bus());
        let runtime_status = std::sync::Arc::new(tokio::sync::Mutex::new(
            MemoryMountRuntimeStatus::configured(
                resolved.mount_path.clone(),
                resolved.project_id.clone(),
            ),
        ));
        let fs = LinuxMemoryFilesystem::new(
            repo,
            resolved.project_id,
            resolved.project_path,
            state.clone(),
            runtime_status.clone(),
        );
        let options = vec![
            fuser::MountOption::FSName("djinn-memory".to_string()),
            fuser::MountOption::DefaultPermissions,
            fuser::MountOption::AutoUnmount,
            fuser::MountOption::AllowRoot,
        ];
        let session =
            fuser::spawn_mount2(fs, &resolved.mount_path, &options).with_context(|| {
                format!(
                    "failed to mount memory filesystem at {}",
                    resolved.mount_path.display()
                )
            })?;
        runtime_status.lock().await.mark_mounted();
        tracing::info!(mount_path = %resolved.mount_path.display(), "memory filesystem mounted");
        Ok(Some(MountedMemoryFilesystem::from_session(
            session,
            runtime_status,
        )))
    }

    #[cfg(not(all(target_os = "linux", feature = "memory-mount")))]
    {
        let _ = state;
        let _ = resolved;
        bail!("memory mount support is unavailable in this build")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedMemoryMount {
    pub mount_path: PathBuf,
    pub project_id: String,
    pub project_path: PathBuf,
}

#[cfg(all(target_os = "linux", feature = "memory-mount"))]
const TTL: Duration = Duration::from_secs(1);

#[cfg(all(target_os = "linux", feature = "memory-mount"))]
struct LinuxMemoryFilesystem {
    core: Arc<MemoryFilesystemCore>,
    project_id: String,
    project_path: PathBuf,
    state: crate::server::AppState,
    runtime: tokio::runtime::Handle,
    file_handles: Arc<Mutex<HashMap<u64, String>>>,
    pending_writes: Arc<Mutex<HashMap<String, PendingWrite>>>,
    runtime_status: std::sync::Arc<tokio::sync::Mutex<MemoryMountRuntimeStatus>>,
    debounce_window: Duration,
    next_handle: std::sync::atomic::AtomicU64,
}

#[cfg(any(test, all(target_os = "linux", feature = "memory-mount")))]
#[derive(Debug, Clone)]
struct PendingWrite {
    generation: u64,
    content: String,
}

#[cfg(any(test, all(target_os = "linux", feature = "memory-mount")))]
fn apply_write_update(existing: String, offset: i64, data: &[u8]) -> Result<String, i32> {
    let mut bytes = existing.into_bytes();
    let offset = offset.max(0) as usize;
    if bytes.len() < offset {
        bytes.resize(offset, 0);
    }
    if bytes.len() < offset + data.len() {
        bytes.resize(offset + data.len(), 0);
    }
    bytes[offset..offset + data.len()].copy_from_slice(data);
    String::from_utf8(bytes).map_err(|_| libc::EINVAL)
}

#[cfg(any(test, all(target_os = "linux", feature = "memory-mount")))]
fn apply_truncate_update(existing: String, size: u64) -> Result<String, i32> {
    let mut bytes = existing.into_bytes();
    bytes.resize(size as usize, 0);
    String::from_utf8(bytes).map_err(|_| libc::EINVAL)
}

#[cfg(any(test, all(target_os = "linux", feature = "memory-mount")))]
fn stage_pending_content(
    pending_writes: &mut HashMap<String, PendingWrite>,
    path: &str,
    content: String,
) -> u64 {
    let generation = pending_writes
        .get(path)
        .map_or(1, |pending| pending.generation + 1);
    pending_writes.insert(
        path.to_string(),
        PendingWrite {
            generation,
            content,
        },
    );
    generation
}

#[cfg(all(target_os = "linux", feature = "memory-mount"))]
fn remove_pending_if_generation(
    pending_writes: &mut HashMap<String, PendingWrite>,
    path: &str,
    generation: u64,
) -> usize {
    if pending_writes
        .get(path)
        .is_some_and(|candidate| candidate.generation == generation)
    {
        pending_writes.remove(path);
    }

    pending_writes.len()
}

#[cfg(all(target_os = "linux", feature = "memory-mount"))]
impl LinuxMemoryFilesystem {
    fn new(
        repo: NoteRepository,
        project_id: String,
        project_path: PathBuf,
        state: crate::server::AppState,
        runtime_status: std::sync::Arc<tokio::sync::Mutex<MemoryMountRuntimeStatus>>,
    ) -> Self {
        Self {
            core: Arc::new(MemoryFilesystemCore::new(repo)),
            project_id,
            project_path,
            state,
            runtime: tokio::runtime::Handle::current(),
            file_handles: Arc::new(Mutex::new(HashMap::new())),
            pending_writes: Arc::new(Mutex::new(HashMap::new())),
            runtime_status,
            debounce_window: WRITE_DEBOUNCE_WINDOW,
            next_handle: std::sync::atomic::AtomicU64::new(1),
        }
    }

    fn current_view_selection(&self) -> MemoryViewSelection {
        self.runtime.block_on(
            self.state
                .resolve_memory_mount_view_selection(&self.project_id, &self.project_path),
        )
    }

    fn next_fh(&self, path: &str) -> u64 {
        let fh = self
            .next_handle
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.file_handles
            .lock()
            .expect("poisoned file_handles")
            .insert(fh, path.to_string());
        fh
    }

    fn path_for_handle(&self, fh: u64) -> Option<String> {
        self.file_handles
            .lock()
            .expect("poisoned file_handles")
            .get(&fh)
            .cloned()
    }

    fn release_handle(&self, fh: u64) {
        self.file_handles
            .lock()
            .expect("poisoned file_handles")
            .remove(&fh);
    }

    fn inode_for_path(path: &str) -> u64 {
        if path.is_empty() {
            return 1;
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        path.hash(&mut hasher);
        let ino = hasher.finish();
        if ino == 1 { 2 } else { ino }
    }

    fn path_for_inode(&self, ino: u64) -> Result<String, i32> {
        if ino == 1 {
            return Ok(String::new());
        }
        let entries = self.collect_paths().map_err(repo_err_to_errno)?;
        entries
            .into_iter()
            .find(|path| Self::inode_for_path(path) == ino)
            .ok_or(libc::ENOENT)
    }

    fn collect_paths(&self) -> Result<Vec<String>> {
        fn visit(
            fs: &LinuxMemoryFilesystem,
            selection: &MemoryViewSelection,
            path: &str,
            entries: &mut Vec<String>,
        ) -> Result<()> {
            entries.push(path.to_string());
            let metadata = fs
                .runtime
                .block_on(fs.core.stat_in_view(&fs.project_id, selection, path))
                .map_err(|e| anyhow!(e.to_string()))?;
            if metadata.kind != MemoryEntryKind::Directory {
                return Ok(());
            }
            let children = fs
                .runtime
                .block_on(fs.core.list_dir_in_view(&fs.project_id, selection, path))
                .map_err(|e| anyhow!(e.to_string()))?;
            for child in children {
                visit(fs, selection, &child.metadata.path, entries)?;
            }
            Ok(())
        }

        let mut entries = Vec::new();
        let selection = self.current_view_selection();
        visit(self, &selection, "", &mut entries)?;
        Ok(entries)
    }

    fn attr_for_path(&self, path: &str) -> Result<fuser::FileAttr, i32> {
        if let Some(content) = self.pending_content(path) {
            return self.staged_attr_for_path(path, &content);
        }
        let selection = self.current_view_selection();
        let metadata = self
            .runtime
            .block_on(self.core.stat_in_view(&self.project_id, &selection, path))
            .map_err(repo_err_to_errno)?;
        Ok(file_attr_for_metadata(&metadata))
    }

    fn child_path(parent: &str, name: &OsStr) -> String {
        let name = name.to_string_lossy();
        if parent.is_empty() {
            name.into_owned()
        } else {
            format!("{parent}/{name}")
        }
    }

    fn pending_content(&self, path: &str) -> Option<String> {
        self.pending_writes
            .lock()
            .expect("poisoned pending_writes")
            .get(path)
            .map(|pending| pending.content.clone())
    }

    fn update_pending_count(&self) {
        let count = self
            .pending_writes
            .lock()
            .expect("poisoned pending_writes")
            .len();
        self.runtime.block_on(async {
            self.runtime_status.lock().await.set_pending_writes(count);
        });
    }

    fn staged_attr_for_path(&self, path: &str, content: &str) -> Result<fuser::FileAttr, i32> {
        let selection = self.current_view_selection();
        let mut metadata = self
            .runtime
            .block_on(self.core.stat_in_view(&self.project_id, &selection, path))
            .unwrap_or(MemoryEntryMetadata {
                path: path.to_string(),
                kind: MemoryEntryKind::File,
                size: 0,
            });
        metadata.size = content.len() as u64;
        Ok(file_attr_for_metadata(&metadata))
    }

    fn schedule_debounced_flush(&self, path: String, generation: u64) {
        let pending_writes = self.pending_writes.clone();
        let runtime_status = self.runtime_status.clone();
        let core = self.core.clone();
        let project_id = self.project_id.clone();
        let project_path = self.project_path.clone();
        let state = self.state.clone();
        let debounce_window = self.debounce_window;

        self.runtime.spawn(async move {
            tokio::time::sleep(debounce_window).await;
            let pending = {
                let guard = pending_writes.lock().expect("poisoned pending_writes");
                match guard.get(&path) {
                    Some(candidate) if candidate.generation == generation => {
                        Some(candidate.clone())
                    }
                    _ => None,
                }
            };

            let Some(pending) = pending else {
                return;
            };

            let selection = state
                .resolve_memory_mount_view_selection(&project_id, &project_path)
                .await;
            let flush_result = core
                .write_file_in_view(
                    &project_id,
                    &selection,
                    &project_path,
                    &path,
                    &pending.content,
                )
                .await;

            let pending_count = {
                let mut guard = pending_writes.lock().expect("poisoned pending_writes");
                remove_pending_if_generation(&mut guard, &path, generation)
            };

            let mut status = runtime_status.lock().await;
            status.set_pending_writes(pending_count);
            match flush_result {
                Ok(_) => {
                    if matches!(
                        status.lifecycle,
                        crate::server::MemoryMountLifecycleState::Configured
                            | crate::server::MemoryMountLifecycleState::Degraded
                    ) {
                        status.mark_mounted();
                    }
                }
                Err(err) => status
                    .mark_degraded(format!("failed to flush debounced write for {path}: {err}")),
            }
        });
    }

    fn queue_write(&self, path: &str, offset: i64, data: &[u8]) -> Result<u32, i32> {
        let selection = self.current_view_selection();
        let existing = self.pending_content(path).unwrap_or_else(|| {
            self.runtime
                .block_on(
                    self.core
                        .read_file_in_view(&self.project_id, &selection, path),
                )
                .map(|file| file.content)
                .unwrap_or_default()
        });

        let content = apply_write_update(existing, offset, data)?;

        let generation = {
            let mut guard = self.pending_writes.lock().expect("poisoned pending_writes");
            stage_pending_content(&mut guard, path, content)
        };
        self.update_pending_count();
        self.schedule_debounced_flush(path.to_string(), generation);
        Ok(data.len() as u32)
    }

    fn queue_truncate(&self, path: &str, size: u64) -> Result<fuser::FileAttr, i32> {
        let selection = self.current_view_selection();
        let existing = self.pending_content(path).unwrap_or_else(|| {
            self.runtime
                .block_on(
                    self.core
                        .read_file_in_view(&self.project_id, &selection, path),
                )
                .map(|file| file.content)
                .unwrap_or_default()
        });
        let content = apply_truncate_update(existing, size)?;

        let generation = {
            let mut guard = self.pending_writes.lock().expect("poisoned pending_writes");
            stage_pending_content(&mut guard, path, content.clone())
        };
        self.update_pending_count();
        self.schedule_debounced_flush(path.to_string(), generation);
        self.staged_attr_for_path(path, &content)
    }

    fn flush_pending_write(&self, path: &str) -> Result<(), i32> {
        let pending = self
            .pending_writes
            .lock()
            .expect("poisoned pending_writes")
            .remove(path);
        self.update_pending_count();

        let Some(pending) = pending else {
            return Ok(());
        };

        let selection = self.current_view_selection();
        match self.runtime.block_on(self.core.write_file_in_view(
            &self.project_id,
            &selection,
            &self.project_path,
            path,
            &pending.content,
        )) {
            Ok(_) => {
                self.runtime.block_on(async {
                    self.runtime_status.lock().await.mark_mounted();
                });
                Ok(())
            }
            Err(err) => {
                self.runtime.block_on(async {
                    self.runtime_status
                        .lock()
                        .await
                        .mark_degraded(format!("failed to flush pending write for {path}: {err}"));
                });
                Err(repo_err_to_errno(err))
            }
        }
    }
}

#[cfg(all(target_os = "linux", feature = "memory-mount"))]
impl fuser::Filesystem for LinuxMemoryFilesystem {
    fn lookup(
        &mut self,
        _req: &fuser::Request<'_>,
        parent: u64,
        name: &OsStr,
        reply: fuser::ReplyEntry,
    ) {
        let parent_path = match self.path_for_inode(parent) {
            Ok(path) => path,
            Err(errno) => return reply.error(errno),
        };
        let child_path = Self::child_path(&parent_path, name);
        match self.attr_for_path(&child_path) {
            Ok(attr) => reply.entry(&TTL, &attr, 0),
            Err(errno) => reply.error(errno),
        }
    }

    fn getattr(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        _fh: Option<u64>,
        reply: fuser::ReplyAttr,
    ) {
        let path = match self.path_for_inode(ino) {
            Ok(path) => path,
            Err(errno) => return reply.error(errno),
        };
        match self.attr_for_path(&path) {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(errno) => reply.error(errno),
        }
    }

    fn access(
        &mut self,
        _req: &fuser::Request<'_>,
        _ino: u64,
        _mask: i32,
        reply: fuser::ReplyEmpty,
    ) {
        reply.ok();
    }

    fn open(&mut self, _req: &fuser::Request<'_>, ino: u64, _flags: i32, reply: fuser::ReplyOpen) {
        let path = match self.path_for_inode(ino) {
            Ok(path) => path,
            Err(errno) => return reply.error(errno),
        };
        let fh = self.next_fh(&path);
        reply.opened(fh, 0);
    }

    fn release(
        &mut self,
        _req: &fuser::Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: fuser::ReplyEmpty,
    ) {
        let path = self.path_for_handle(fh);
        self.release_handle(fh);
        if let Some(path) = path
            && let Err(errno) = self.flush_pending_write(&path)
        {
            reply.error(errno);
            return;
        }
        reply.ok();
    }

    fn read(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: fuser::ReplyData,
    ) {
        let path = self
            .path_for_handle(fh)
            .or_else(|| self.path_for_inode(ino).ok())
            .unwrap_or_default();
        if let Some(content) = self.pending_content(&path) {
            let start = offset.max(0) as usize;
            let bytes = content.as_bytes();
            let end = start.saturating_add(size as usize).min(bytes.len());
            let data = if start >= bytes.len() {
                &[]
            } else {
                &bytes[start..end]
            };
            reply.data(data);
            return;
        }
        let selection = self.current_view_selection();
        let file = match self.runtime.block_on(self.core.read_file_in_view(
            &self.project_id,
            &selection,
            &path,
        )) {
            Ok(file) => file,
            Err(err) => return reply.error(repo_err_to_errno(err)),
        };
        let start = offset.max(0) as usize;
        let bytes = file.content.as_bytes();
        let end = start.saturating_add(size as usize).min(bytes.len());
        let data = if start >= bytes.len() {
            &[]
        } else {
            &bytes[start..end]
        };
        reply.data(data);
    }

    fn write(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: fuser::ReplyWrite,
    ) {
        let path = self
            .path_for_handle(fh)
            .or_else(|| self.path_for_inode(ino).ok())
            .unwrap_or_default();
        match self.queue_write(&path, offset, data) {
            Ok(written) => reply.written(written),
            Err(errno) => reply.error(errno),
        }
    }

    fn setattr(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: fuser::ReplyAttr,
    ) {
        let Some(size) = size else {
            let path = match self.path_for_inode(ino) {
                Ok(path) => path,
                Err(errno) => return reply.error(errno),
            };
            return match self.attr_for_path(&path) {
                Ok(attr) => reply.attr(&TTL, &attr),
                Err(errno) => reply.error(errno),
            };
        };

        let path = fh
            .and_then(|handle| self.path_for_handle(handle))
            .or_else(|| self.path_for_inode(ino).ok())
            .unwrap_or_default();
        match self.queue_truncate(&path, size) {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(errno) => reply.error(errno),
        }
    }

    fn opendir(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        _flags: i32,
        reply: fuser::ReplyOpen,
    ) {
        let path = match self.path_for_inode(ino) {
            Ok(path) => path,
            Err(errno) => return reply.error(errno),
        };
        let fh = self.next_fh(&path);
        reply.opened(fh, 0);
    }

    fn releasedir(
        &mut self,
        _req: &fuser::Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        reply: fuser::ReplyEmpty,
    ) {
        self.release_handle(fh);
        reply.ok();
    }

    fn readdir(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        mut reply: fuser::ReplyDirectory,
    ) {
        let path = self
            .path_for_handle(fh)
            .or_else(|| self.path_for_inode(ino).ok())
            .unwrap_or_default();
        let selection = self.current_view_selection();
        let children = match self.runtime.block_on(self.core.list_dir_in_view(
            &self.project_id,
            &selection,
            &path,
        )) {
            Ok(children) => children,
            Err(err) => return reply.error(repo_err_to_errno(err)),
        };

        let parent = Path::new(&path)
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or("")
            .to_string();
        let mut entries = vec![
            (ino, fuser::FileType::Directory, ".".to_string()),
            (
                Self::inode_for_path(&parent),
                fuser::FileType::Directory,
                "..".to_string(),
            ),
        ];
        entries.extend(children.into_iter().map(|entry| {
            let kind = match entry.metadata.kind {
                MemoryEntryKind::Directory => fuser::FileType::Directory,
                MemoryEntryKind::File => fuser::FileType::RegularFile,
            };
            (Self::inode_for_path(&entry.metadata.path), kind, entry.name)
        }));

        for (idx, (child_ino, kind, name)) in
            entries.into_iter().enumerate().skip(offset.max(0) as usize)
        {
            if reply.add(child_ino, (idx + 1) as i64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn create(
        &mut self,
        _req: &fuser::Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: fuser::ReplyCreate,
    ) {
        let parent_path = match self.path_for_inode(parent) {
            Ok(path) => path,
            Err(errno) => return reply.error(errno),
        };
        let path = Self::child_path(&parent_path, name);
        let selection = self.current_view_selection();
        match self.runtime.block_on(self.core.write_file_in_view(
            &self.project_id,
            &selection,
            &self.project_path,
            &path,
            "",
        )) {
            Ok(file) => {
                let attr = file_attr_for_metadata(&file.metadata);
                let fh = self.next_fh(&file.metadata.path);
                reply.created(&TTL, &attr, 0, fh, 0);
            }
            Err(err) => reply.error(repo_err_to_errno(err)),
        }
    }

    fn unlink(
        &mut self,
        _req: &fuser::Request<'_>,
        parent: u64,
        name: &OsStr,
        reply: fuser::ReplyEmpty,
    ) {
        let parent_path = match self.path_for_inode(parent) {
            Ok(path) => path,
            Err(errno) => return reply.error(errno),
        };
        let path = Self::child_path(&parent_path, name);
        if let Err(errno) = self.flush_pending_write(&path) {
            return reply.error(errno);
        }
        let selection = self.current_view_selection();
        match self.runtime.block_on(self.core.delete_file_in_view(
            &self.project_id,
            &selection,
            &path,
        )) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(repo_err_to_errno(err)),
        }
    }

    fn rename(
        &mut self,
        _req: &fuser::Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: fuser::ReplyEmpty,
    ) {
        let from_parent = match self.path_for_inode(parent) {
            Ok(path) => path,
            Err(errno) => return reply.error(errno),
        };
        let to_parent = match self.path_for_inode(newparent) {
            Ok(path) => path,
            Err(errno) => return reply.error(errno),
        };
        let from_path = Self::child_path(&from_parent, name);
        let to_path = Self::child_path(&to_parent, newname);
        if let Err(errno) = self.flush_pending_write(&from_path) {
            return reply.error(errno);
        }
        let selection = self.current_view_selection();
        match self.runtime.block_on(self.core.rename_file_in_view(
            &self.project_id,
            &selection,
            &self.project_path,
            &from_path,
            &to_path,
        )) {
            Ok(_) => reply.ok(),
            Err(err) => reply.error(repo_err_to_errno(err)),
        }
    }
}

#[cfg(all(target_os = "linux", feature = "memory-mount"))]
fn file_attr_for_metadata(metadata: &crate::memory_fs::MemoryEntryMetadata) -> fuser::FileAttr {
    let kind = match metadata.kind {
        MemoryEntryKind::Directory => fuser::FileType::Directory,
        MemoryEntryKind::File => fuser::FileType::RegularFile,
    };
    let perm = match metadata.kind {
        MemoryEntryKind::Directory => 0o755,
        MemoryEntryKind::File => 0o644,
    };
    let now = SystemTime::now();
    fuser::FileAttr {
        ino: LinuxMemoryFilesystem::inode_for_path(&metadata.path),
        size: metadata.size,
        blocks: metadata.size.div_ceil(512),
        atime: now,
        mtime: now,
        ctime: now,
        crtime: now,
        kind,
        perm,
        nlink: if metadata.kind == MemoryEntryKind::Directory {
            2
        } else {
            1
        },
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        rdev: 0,
        blksize: 512,
        flags: 0,
    }
}

#[cfg(all(target_os = "linux", feature = "memory-mount"))]
fn repo_err_to_errno(error: impl std::fmt::Display) -> i32 {
    let message = error.to_string();
    if message.contains("path not found") {
        libc::ENOENT
    } else if message.contains("not a directory") {
        libc::ENOTDIR
    } else if message.contains("not a file") {
        libc::EISDIR
    } else if message.contains("path already exists") {
        libc::EEXIST
    } else {
        libc::EIO
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use crate::memory_fs::MemoryViewSelection;
    use crate::server::AppState;
    use crate::test_helpers::{
        create_test_db, create_test_epic, create_test_project_with_dir, create_test_task,
        test_events, workspace_tempdir,
    };
    use djinn_db::{CreateSessionParams, SessionRepository};
    use tokio_util::sync::CancellationToken;

    #[cfg(all(target_os = "linux", feature = "memory-mount"))]
    async fn build_runtime_test_filesystem(
        initial_content: &str,
    ) -> (LinuxMemoryFilesystem, String, tempfile::TempDir) {
        let db = create_test_db();
        let state = AppState::new(db.clone(), CancellationToken::new());
        let (project, project_dir) = create_test_project_with_dir(&db).await;
        let repo = djinn_db::NoteRepository::new(db.clone(), test_events());
        let note = repo
            .create(
                &project.id,
                project_dir.path(),
                "runtime batched note",
                initial_content,
                "research",
                "[]",
            )
            .await
            .expect("create runtime test note");
        let runtime_status = std::sync::Arc::new(tokio::sync::Mutex::new(
            MemoryMountRuntimeStatus::configured(
                project_dir.path().join("mount"),
                project.id.clone(),
            ),
        ));
        let fs = LinuxMemoryFilesystem::new(
            repo,
            project.id,
            project_dir.path().to_path_buf(),
            state,
            runtime_status,
        );
        (
            fs,
            djinn_db::virtual_note_path_for_permalink(&note.permalink),
            project_dir,
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disabled_mount_settings_skip_validation() {
        let settings = DjinnSettings::default();
        let result = validate_mount_config(&settings, create_test_db(), test_events())
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enabled_mount_requires_absolute_path() {
        let settings = DjinnSettings {
            memory_mount_enabled: Some(true),
            memory_mount_path: Some("relative/path".to_string()),
            ..Default::default()
        };
        let error = validate_mount_config(&settings, create_test_db(), test_events())
            .await
            .expect_err("relative path should fail");

        #[cfg(all(target_os = "linux", feature = "memory-mount"))]
        assert!(error.to_string().contains("absolute path"));

        #[cfg(not(all(target_os = "linux", feature = "memory-mount")))]
        assert!(error.to_string().contains(
            "memory mount is enabled in settings but djinn-server was built without the `memory-mount` feature"
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enabled_mount_requires_mount_path() {
        let settings = DjinnSettings {
            memory_mount_enabled: Some(true),
            memory_mount_path: None,
            ..Default::default()
        };
        let error = validate_mount_config(&settings, create_test_db(), test_events())
            .await
            .expect_err("missing mount path should fail");

        #[cfg(all(target_os = "linux", feature = "memory-mount"))]
        assert!(
            error
                .to_string()
                .contains("memory_mount_path must be set when memory_mount_enabled is true")
        );

        #[cfg(not(all(target_os = "linux", feature = "memory-mount")))]
        assert!(
            error.to_string().contains(
                "memory mount is enabled in settings but djinn-server was built without the `memory-mount` feature"
            ) || error
                .to_string()
                .contains("memory mount is only supported on Linux in ADR-057 wave 1")
        );
    }

    #[test]
    fn staged_write_updates_are_coalesced_by_path_and_generation() {
        let mut pending = HashMap::new();

        let first = stage_pending_content(
            &mut pending,
            "research/batched-note.md",
            "first".to_string(),
        );
        let second = stage_pending_content(
            &mut pending,
            "research/batched-note.md",
            "second".to_string(),
        );

        assert_eq!(first, 1);
        assert_eq!(second, 2);
        assert_eq!(pending.len(), 1);
        let pending_write = pending
            .get("research/batched-note.md")
            .expect("pending write should be retained");
        assert_eq!(pending_write.generation, 2);
        assert_eq!(pending_write.content, "second");
    }

    #[test]
    fn apply_write_update_batches_successive_write_bursts() {
        let content = apply_write_update(String::new(), 0, b"hello").expect("initial write");
        let content = apply_write_update(content, 5, b" world").expect("append write");
        let content = apply_write_update(content, 6, b"Djinn").expect("overwrite write");

        assert_eq!(content, "hello Djinn");
    }

    #[test]
    fn apply_truncate_update_preserves_create_update_truncate_flow() {
        let created = apply_write_update(String::new(), 0, b"abcdef").expect("create content");
        let truncated = apply_truncate_update(created, 3).expect("truncate content");
        let regrown = apply_truncate_update(truncated.clone(), 5).expect("grow content");

        assert_eq!(truncated, "abc");
        assert_eq!(regrown.as_bytes(), b"abc\0\0");
    }

    #[test]
    fn runtime_status_tracks_pending_write_and_degraded_health_details() {
        let mut status = MemoryMountRuntimeStatus::configured(
            PathBuf::from("/mnt/djinn-memory"),
            "project-123".to_string(),
        );

        status.set_pending_writes(2);
        status.mark_degraded("debounced flush failed");

        assert_eq!(
            status.lifecycle,
            crate::server::MemoryMountLifecycleState::Degraded
        );
        assert!(status.configured);
        assert_eq!(status.pending_writes, 2);
        assert_eq!(status.detail.as_deref(), Some("debounced flush failed"));
        assert_eq!(status.last_error.as_deref(), Some("debounced flush failed"));
        assert_eq!(
            status.mount_path.as_deref(),
            Some(std::path::Path::new("/mnt/djinn-memory"))
        );
        assert_eq!(status.project_id.as_deref(), Some("project-123"));
    }

    #[test]
    fn staged_write_and_truncate_bursts_share_one_pending_entry_until_flush() {
        let mut pending = HashMap::new();

        let content = apply_write_update(String::new(), 0, b"hello").expect("initial write");
        let first_generation =
            stage_pending_content(&mut pending, "research/batched-note.md", content);
        assert_eq!(first_generation, 1);
        assert_eq!(pending.len(), 1);

        let content = apply_write_update(
            pending["research/batched-note.md"].content.clone(),
            5,
            b" world",
        )
        .expect("successive write burst");
        let second_generation =
            stage_pending_content(&mut pending, "research/batched-note.md", content);
        assert_eq!(second_generation, 2);
        assert_eq!(pending.len(), 1);

        let content = apply_truncate_update(pending["research/batched-note.md"].content.clone(), 5)
            .expect("truncate burst");
        let third_generation =
            stage_pending_content(&mut pending, "research/batched-note.md", content);
        assert_eq!(third_generation, 3);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending["research/batched-note.md"].content, "hello");

        let flushed = pending
            .remove("research/batched-note.md")
            .expect("flush removes the coalesced pending write");
        assert_eq!(flushed.generation, 3);
        assert_eq!(flushed.content, "hello");
        assert!(pending.is_empty());
    }

    #[test]
    fn runtime_status_recovers_to_mounted_after_successful_flush() {
        let mut status = MemoryMountRuntimeStatus::configured(
            PathBuf::from("/mnt/djinn-memory"),
            "project-123".to_string(),
        );

        status.set_pending_writes(1);
        status.mark_degraded("failed to flush pending write for research/note.md: boom");
        assert_eq!(status.pending_writes, 1);
        assert_eq!(
            status.last_error.as_deref(),
            Some("failed to flush pending write for research/note.md: boom")
        );

        status.set_pending_writes(0);
        status.mark_mounted();

        assert_eq!(
            status.lifecycle,
            crate::server::MemoryMountLifecycleState::Mounted
        );
        assert_eq!(status.pending_writes, 0);
        assert_eq!(status.detail, None);
        assert_eq!(
            status.last_error.as_deref(),
            Some("failed to flush pending write for research/note.md: boom")
        );
    }

    #[cfg(all(target_os = "linux", feature = "memory-mount"))]
    #[test]
    fn runtime_queue_write_and_truncate_coalesce_through_flush_path() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build runtime");
        let (fs, path, _project_dir) = runtime.block_on(build_runtime_test_filesystem("hello"));
        let initial_file = runtime
            .block_on(fs.core.read_file_in_view(
                &fs.project_id,
                &MemoryViewSelection::Canonical,
                &path,
            ))
            .expect("read initial file");
        let initial_content = initial_file.content;

        assert_eq!(runtime.block_on(fs.runtime_status.lock()).pending_writes, 0);

        fs.queue_write(&path, initial_content.len() as i64, b"\nqueued burst")
            .expect("stage append write");
        assert_eq!(runtime.block_on(fs.runtime_status.lock()).pending_writes, 1);
        let expected_burst = format!("{initial_content}\nqueued burst");
        assert_eq!(
            fs.pending_content(&path).as_deref(),
            Some(expected_burst.as_str())
        );

        fs.queue_truncate(&path, initial_content.len() as u64)
            .expect("stage truncate");
        assert_eq!(runtime.block_on(fs.runtime_status.lock()).pending_writes, 1);
        assert_eq!(
            fs.pending_content(&path).as_deref(),
            Some(initial_content.as_str())
        );

        fs.flush_pending_write(&path)
            .expect("flush coalesced pending write");

        let status = runtime.block_on(fs.runtime_status.lock()).clone();
        assert_eq!(status.pending_writes, 0);
        assert_eq!(
            status.lifecycle,
            crate::server::MemoryMountLifecycleState::Mounted
        );
        assert_eq!(status.detail, None);

        let file = runtime
            .block_on(fs.core.read_file_in_view(
                &fs.project_id,
                &MemoryViewSelection::Canonical,
                &path,
            ))
            .expect("read flushed file");
        assert_eq!(file.content, initial_content);
    }

    #[cfg(all(target_os = "linux", feature = "memory-mount"))]
    #[test]
    fn runtime_flush_failure_updates_degraded_status_from_real_flush_logic() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build runtime");
        let (fs, path, _project_dir) = runtime.block_on(build_runtime_test_filesystem("seed"));
        let invalid_path = "research";

        fs.queue_write(invalid_path, 0, b"broken")
            .expect("stage write before failed flush");
        assert_eq!(runtime.block_on(fs.runtime_status.lock()).pending_writes, 1);

        let errno = fs
            .flush_pending_write(invalid_path)
            .expect_err("flush should fail for a directory path");
        assert_eq!(errno, libc::EISDIR);

        let status = runtime.block_on(fs.runtime_status.lock()).clone();
        assert_eq!(status.pending_writes, 0);
        assert_eq!(
            status.lifecycle,
            crate::server::MemoryMountLifecycleState::Degraded
        );
        let detail = status.detail.expect("degraded detail should be populated");
        assert!(detail.contains("failed to flush pending write for"));
        assert!(detail.contains(invalid_path));
        assert_eq!(status.last_error.as_deref(), Some(detail.as_str()));

        let canonical_file = runtime.block_on(fs.core.read_file_in_view(
            &fs.project_id,
            &MemoryViewSelection::Canonical,
            &path,
        ));
        assert!(
            canonical_file.is_ok(),
            "existing note remains readable after failed flush"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runtime_branch_selection_uses_active_task_session_worktree() {
        let db = create_test_db();
        let state = AppState::new(db.clone(), CancellationToken::new());
        let (project, _project_dir) = create_test_project_with_dir(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let task = create_test_task(&db, &project.id, &epic.id).await;
        let worktree_dir = workspace_tempdir("memory-mount-task-worktree-");

        SessionRepository::new(db.clone(), test_events())
            .create(CreateSessionParams {
                project_id: &project.id,
                task_id: Some(&task.id),
                model: "test-model",
                agent_type: "worker",
                worktree_path: Some(&worktree_dir.path().to_string_lossy()),
                metadata_json: None,
            })
            .await
            .expect("create running session");

        state.agent_context().register_activity(&task.id);

        let selection = state
            .resolve_memory_mount_view_selection(&project.id, Path::new(&project.path))
            .await;

        assert_eq!(
            selection,
            MemoryViewSelection::Task {
                task_short_id: Some(task.short_id),
                worktree_root: Some(worktree_dir.path().to_path_buf()),
            }
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runtime_branch_selection_falls_back_without_running_session_context() {
        let db = create_test_db();
        let state = AppState::new(db.clone(), CancellationToken::new());
        let (project, _project_dir) = create_test_project_with_dir(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let task = create_test_task(&db, &project.id, &epic.id).await;

        state.agent_context().register_activity(&task.id);

        let selection = state
            .resolve_memory_mount_view_selection(&project.id, Path::new(&project.path))
            .await;

        assert_eq!(selection, MemoryViewSelection::Canonical);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runtime_branch_selection_falls_back_when_active_task_branch_is_ambiguous() {
        let db = create_test_db();
        let state = AppState::new(db.clone(), CancellationToken::new());
        let (project, _project_dir) = create_test_project_with_dir(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let first = create_test_task(&db, &project.id, &epic.id).await;
        let second = create_test_task(&db, &project.id, &epic.id).await;

        let agent = state.agent_context();
        agent.register_activity(&first.id);
        agent.register_activity(&second.id);

        let selection = state
            .resolve_memory_mount_view_selection(&project.id, Path::new(&project.path))
            .await;

        assert_eq!(selection, MemoryViewSelection::Canonical);
    }
}
