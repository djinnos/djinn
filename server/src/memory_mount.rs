//! Linux-only FUSE adapter for the transport-neutral [`crate::memory_fs::MemoryFilesystemCore`].
//!
//! This adapter is disabled by default behind the `linux-fuse-memory-mount` cargo feature and a
//! runtime settings gate. When enabled, it mounts one configured project's memory note tree at
//! `<project>/.djinn/memory` by default, or at an explicit absolute path override inside the same
//! project root.
//!
//! Current scope is intentionally narrow:
//!
//! - Linux only
//! - single configured project mount
//! - startup-time validation and explicit opt-in
//! - repository-backed read/list/stat/write/rename/delete translation
//!
//! Deferred to later ADR-057 waves:
//!
//! - macOS / NFS fallback
//! - branch-aware session switching
//! - debounced write batching and richer mount lifecycle reporting

use std::path::PathBuf;

use djinn_db::ProjectRepository;
use thiserror::Error;

use crate::server::AppState;

#[cfg(all(target_os = "linux", feature = "linux-fuse-memory-mount"))]
use fuser::{
    BackgroundSession, FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate,
    ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request,
};
#[cfg(all(target_os = "linux", feature = "linux-fuse-memory-mount"))]
use std::collections::HashMap;
#[cfg(all(target_os = "linux", feature = "linux-fuse-memory-mount"))]
use std::ffi::OsStr;
#[cfg(all(target_os = "linux", feature = "linux-fuse-memory-mount"))]
use std::path::Path;
#[cfg(all(target_os = "linux", feature = "linux-fuse-memory-mount"))]
use std::sync::Arc;
#[cfg(all(target_os = "linux", feature = "linux-fuse-memory-mount"))]
use std::time::{Duration, SystemTime};
#[cfg(all(target_os = "linux", feature = "linux-fuse-memory-mount"))]
use tokio::runtime::Handle;
#[cfg(all(target_os = "linux", feature = "linux-fuse-memory-mount"))]
use tokio::sync::Mutex;

#[cfg(all(target_os = "linux", feature = "linux-fuse-memory-mount"))]
use crate::memory_fs::{MemoryEntryKind, MemoryFilesystemCore};

#[cfg(all(target_os = "linux", feature = "linux-fuse-memory-mount"))]
const TTL: Duration = Duration::from_secs(1);
#[cfg(all(target_os = "linux", feature = "linux-fuse-memory-mount"))]
const ROOT_INO: u64 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryMountConfig {
    pub project_id: String,
    pub mount_path: PathBuf,
}

#[derive(Debug, Error)]
pub enum MemoryMountConfigError {
    #[error("memory mount is only available on Linux")]
    NonLinux,
    #[error("memory mount requires the `linux-fuse-memory-mount` feature")]
    FeatureDisabled,
    #[error("memory mount is enabled but no project id was configured")]
    MissingProjectId,
    #[error("memory mount project not found: {0}")]
    UnknownProject(String),
    #[error("memory mount path must be absolute: {0}")]
    MountPathNotAbsolute(String),
    #[error("memory mount path must stay inside the configured project root: {mount_path}")]
    MountPathOutsideProject { mount_path: String },
    #[error("memory mount path exists and is not a directory: {0}")]
    MountPathNotDirectory(String),
    #[error("memory mount path is missing and parent directory does not exist: {0}")]
    MissingParentDirectory(String),
    #[error(transparent)]
    Repository(#[from] djinn_db::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Error)]
pub enum MemoryMountError {
    #[error(transparent)]
    Config(#[from] MemoryMountConfigError),
    #[error(transparent)]
    Repository(#[from] djinn_db::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub async fn resolve_mount_config(
    state: &AppState,
    settings: &djinn_core::models::DjinnSettings,
) -> Result<Option<MemoryMountConfig>, MemoryMountConfigError> {
    if !settings.memory_mount_enabled.unwrap_or(false) {
        return Ok(None);
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = state;
        return Err(MemoryMountConfigError::NonLinux);
    }

    #[cfg(target_os = "linux")]
    {
        if !cfg!(feature = "linux-fuse-memory-mount") {
            return Err(MemoryMountConfigError::FeatureDisabled);
        }

        let project_id = settings
            .memory_mount_project_id
            .clone()
            .filter(|value| !value.trim().is_empty())
            .ok_or(MemoryMountConfigError::MissingProjectId)?;

        let repo = ProjectRepository::new(state.db().clone(), state.event_bus());
        let project = repo
            .get(&project_id)
            .await?
            .ok_or_else(|| MemoryMountConfigError::UnknownProject(project_id.clone()))?;

        let project_root = PathBuf::from(&project.path);
        let mount_path = if let Some(configured) = settings
            .memory_mount_path
            .clone()
            .filter(|value| !value.trim().is_empty())
        {
            let candidate = PathBuf::from(&configured);
            if !candidate.is_absolute() {
                return Err(MemoryMountConfigError::MountPathNotAbsolute(configured));
            }
            candidate
        } else {
            project_root.join(".djinn/memory")
        };

        if !mount_path.starts_with(&project_root) {
            return Err(MemoryMountConfigError::MountPathOutsideProject {
                mount_path: mount_path.display().to_string(),
            });
        }

        if mount_path.exists() {
            if !mount_path.is_dir() {
                return Err(MemoryMountConfigError::MountPathNotDirectory(
                    mount_path.display().to_string(),
                ));
            }
        } else {
            let parent = mount_path.parent().ok_or_else(|| {
                MemoryMountConfigError::MissingParentDirectory(mount_path.display().to_string())
            })?;
            if !parent.exists() {
                return Err(MemoryMountConfigError::MissingParentDirectory(
                    parent.display().to_string(),
                ));
            }
            std::fs::create_dir_all(&mount_path)?;
        }

        Ok(Some(MemoryMountConfig {
            project_id,
            mount_path,
        }))
    }
}

#[cfg(any(not(target_os = "linux"), not(feature = "linux-fuse-memory-mount")))]
pub async fn ensure_memory_mount(
    state: &AppState,
    settings: &djinn_core::models::DjinnSettings,
) -> Result<(), MemoryMountError> {
    let _ = resolve_mount_config(state, settings).await?;
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "linux-fuse-memory-mount"))]
pub async fn ensure_memory_mount(
    state: &AppState,
    settings: &djinn_core::models::DjinnSettings,
) -> Result<(), MemoryMountError> {
    let desired = resolve_mount_config(state, settings).await?;
    state.reconcile_memory_mount(desired).await
}

#[cfg(all(target_os = "linux", feature = "linux-fuse-memory-mount"))]
pub(crate) struct MountedMemoryFilesystem {
    pub(crate) config: MemoryMountConfig,
    pub(crate) _session: BackgroundSession,
}

#[cfg(all(target_os = "linux", feature = "linux-fuse-memory-mount"))]
pub(crate) async fn start_memory_mount(
    state: &AppState,
    config: MemoryMountConfig,
) -> Result<MountedMemoryFilesystem, MemoryMountError> {
    let repo = NoteRepository::new(state.db().clone(), state.event_bus());
    let project_repo = ProjectRepository::new(state.db().clone(), state.event_bus());
    let project = project_repo
        .get(&config.project_id)
        .await?
        .ok_or_else(|| MemoryMountConfigError::UnknownProject(config.project_id.clone()))?;
    let project_root = PathBuf::from(project.path);

    let fs = DjinnFuseFilesystem::new(
        Handle::current(),
        config.project_id.clone(),
        project_root,
        MemoryFilesystemCore::new(repo),
    );

    let session = fuser::spawn_mount2(
        fs,
        &config.mount_path,
        &[
            MountOption::FSName("djinn-memory".to_string()),
            MountOption::Subtype("djinn-memory".to_string()),
            MountOption::AllowOther,
            MountOption::AutoUnmount,
            MountOption::DefaultPermissions,
            MountOption::RW,
        ],
    )?;

    tracing::info!(
        project_id = %config.project_id,
        mount_path = %config.mount_path.display(),
        "memory FUSE mount started"
    );

    Ok(MountedMemoryFilesystem {
        config,
        _session: session,
    })
}

#[cfg(all(target_os = "linux", feature = "linux-fuse-memory-mount"))]
struct DjinnFuseFilesystem {
    rt: Handle,
    project_id: String,
    project_root: PathBuf,
    core: MemoryFilesystemCore,
    state: Arc<Mutex<FuseState>>,
}

#[cfg(all(target_os = "linux", feature = "linux-fuse-memory-mount"))]
struct FuseState {
    next_ino: u64,
    next_handle: u64,
    path_to_ino: HashMap<String, u64>,
    ino_to_path: HashMap<u64, String>,
    file_handles: HashMap<u64, FileHandleState>,
}

#[cfg(all(target_os = "linux", feature = "linux-fuse-memory-mount"))]
struct FileHandleState {
    path: String,
    content: Vec<u8>,
    dirty: bool,
}

#[cfg(all(target_os = "linux", feature = "linux-fuse-memory-mount"))]
impl FuseState {
    fn new() -> Self {
        let mut path_to_ino = HashMap::new();
        let mut ino_to_path = HashMap::new();
        path_to_ino.insert(String::new(), ROOT_INO);
        ino_to_path.insert(ROOT_INO, String::new());
        Self {
            next_ino: ROOT_INO + 1,
            next_handle: 1,
            path_to_ino,
            ino_to_path,
            file_handles: HashMap::new(),
        }
    }

    fn path_for_ino(&self, ino: u64) -> Option<String> {
        self.ino_to_path.get(&ino).cloned()
    }

    fn inode_for_path(&mut self, path: &str) -> u64 {
        if let Some(ino) = self.path_to_ino.get(path) {
            return *ino;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.path_to_ino.insert(path.to_string(), ino);
        self.ino_to_path.insert(ino, path.to_string());
        ino
    }

    fn remove_path(&mut self, path: &str) {
        if let Some(ino) = self.path_to_ino.remove(path) {
            self.ino_to_path.remove(&ino);
        }
    }
}

#[cfg(all(target_os = "linux", feature = "linux-fuse-memory-mount"))]
impl DjinnFuseFilesystem {
    fn new(
        rt: Handle,
        project_id: String,
        project_root: PathBuf,
        core: MemoryFilesystemCore,
    ) -> Self {
        Self {
            rt,
            project_id,
            project_root,
            core,
            state: Arc::new(Mutex::new(FuseState::new())),
        }
    }

    fn join_child(parent: &str, name: &OsStr) -> Option<String> {
        let name = name.to_str()?;
        Some(if parent.is_empty() {
            name.to_string()
        } else {
            format!("{parent}/{name}")
        })
    }

    fn file_attr(&self, ino: u64, kind: MemoryEntryKind, size: u64, req: &Request<'_>) -> FileAttr {
        let now = SystemTime::now();
        FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            kind: match kind {
                MemoryEntryKind::Directory => FileType::Directory,
                MemoryEntryKind::File => FileType::RegularFile,
            },
            perm: match kind {
                MemoryEntryKind::Directory => 0o755,
                MemoryEntryKind::File => 0o644,
            },
            nlink: match kind {
                MemoryEntryKind::Directory => 2,
                MemoryEntryKind::File => 1,
            },
            uid: req.uid(),
            gid: req.gid(),
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    fn write_buffer(handle: &mut FileHandleState, offset: i64, data: &[u8]) -> u32 {
        let offset = offset.max(0) as usize;
        let end = offset + data.len();
        if handle.content.len() < end {
            handle.content.resize(end, 0);
        }
        handle.content[offset..end].copy_from_slice(data);
        handle.dirty = true;
        data.len() as u32
    }
}

#[cfg(all(target_os = "linux", feature = "linux-fuse-memory-mount"))]
impl Filesystem for DjinnFuseFilesystem {
    fn lookup(&mut self, req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(parent_path) = self
            .rt
            .block_on(async { self.state.lock().await.path_for_ino(parent) })
        else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(path) = Self::join_child(&parent_path, name) else {
            reply.error(libc::EINVAL);
            return;
        };

        match self.rt.block_on(self.core.stat(&self.project_id, &path)) {
            Ok(meta) => {
                let ino = self
                    .rt
                    .block_on(async { self.state.lock().await.inode_for_path(&path) });
                reply.entry(&TTL, &self.file_attr(ino, meta.kind, meta.size, req), 0);
            }
            Err(_) => reply.error(libc::ENOENT),
        }
    }

    fn getattr(&mut self, req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let Some(path) = self
            .rt
            .block_on(async { self.state.lock().await.path_for_ino(ino) })
        else {
            reply.error(libc::ENOENT);
            return;
        };
        match self.rt.block_on(self.core.stat(&self.project_id, &path)) {
            Ok(meta) => reply.attr(&TTL, &self.file_attr(ino, meta.kind, meta.size, req)),
            Err(_) => reply.error(libc::ENOENT),
        }
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        let Some(path) = self
            .rt
            .block_on(async { self.state.lock().await.path_for_ino(ino) })
        else {
            reply.error(libc::ENOENT);
            return;
        };
        let file = match self
            .rt
            .block_on(self.core.read_file(&self.project_id, &path))
        {
            Ok(file) => file,
            Err(_) => {
                reply.error(libc::ENOENT);
                return;
            }
        };
        let fh = self.rt.block_on(async {
            let mut state = self.state.lock().await;
            let id = state.next_handle;
            state.next_handle += 1;
            state.file_handles.insert(
                id,
                FileHandleState {
                    path,
                    content: file.content.into_bytes(),
                    dirty: false,
                },
            );
            id
        });
        reply.opened(fh, 0);
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let data = self.rt.block_on(async {
            let state = self.state.lock().await;
            let handle = state.file_handles.get(&fh)?;
            let start = (offset.max(0) as usize).min(handle.content.len());
            let end = (start + size as usize).min(handle.content.len());
            Some(handle.content[start..end].to_vec())
        });
        match data {
            Some(bytes) => reply.data(&bytes),
            None => reply.error(libc::EBADF),
        }
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let written = self.rt.block_on(async {
            let mut state = self.state.lock().await;
            let handle = state.file_handles.get_mut(&fh)?;
            Some(Self::write_buffer(handle, offset, data))
        });
        match written {
            Some(bytes) => reply.written(bytes),
            None => reply.error(libc::EBADF),
        }
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let handle = self
            .rt
            .block_on(async { self.state.lock().await.file_handles.remove(&fh) });
        let Some(handle) = handle else {
            reply.error(libc::EBADF);
            return;
        };

        if handle.dirty {
            match self.rt.block_on(self.core.write_file(
                &self.project_id,
                &self.project_root,
                &handle.path,
                &String::from_utf8_lossy(&handle.content),
            )) {
                Ok(_) => reply.ok(),
                Err(_) => reply.error(libc::EIO),
            }
        } else {
            reply.ok();
        }
    }

    fn opendir(&mut self, _req: &Request<'_>, _ino: u64, _flags: i32, reply: ReplyOpen) {
        reply.opened(0, 0);
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let Some(path) = self
            .rt
            .block_on(async { self.state.lock().await.path_for_ino(ino) })
        else {
            reply.error(libc::ENOENT);
            return;
        };
        let entries = match self
            .rt
            .block_on(self.core.list_dir(&self.project_id, &path))
        {
            Ok(entries) => entries,
            Err(_) => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        let parent_ino = if path.is_empty() {
            ROOT_INO
        } else {
            let parent = Path::new(&path)
                .parent()
                .and_then(|p| p.to_str())
                .unwrap_or("")
                .to_string();
            self.rt
                .block_on(async { self.state.lock().await.inode_for_path(&parent) })
        };

        let mut all = vec![
            (ino, FileType::Directory, ".".to_string()),
            (parent_ino, FileType::Directory, "..".to_string()),
        ];

        for entry in entries {
            let entry_ino = self
                .rt
                .block_on(async { self.state.lock().await.inode_for_path(&entry.metadata.path) });
            all.push((
                entry_ino,
                match entry.metadata.kind {
                    MemoryEntryKind::Directory => FileType::Directory,
                    MemoryEntryKind::File => FileType::RegularFile,
                },
                entry.name,
            ));
        }

        for (idx, (entry_ino, kind, name)) in
            all.into_iter().enumerate().skip(offset.max(0) as usize)
        {
            if reply.add(entry_ino, (idx + 1) as i64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn create(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let Some(parent_path) = self
            .rt
            .block_on(async { self.state.lock().await.path_for_ino(parent) })
        else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(path) = Self::join_child(&parent_path, name) else {
            reply.error(libc::EINVAL);
            return;
        };

        let file = match self.rt.block_on(self.core.write_file(
            &self.project_id,
            &self.project_root,
            &path,
            "",
        )) {
            Ok(file) => file,
            Err(_) => {
                reply.error(libc::EIO);
                return;
            }
        };

        let (ino, fh) = self.rt.block_on(async {
            let mut state = self.state.lock().await;
            let ino = state.inode_for_path(&file.metadata.path);
            let fh = state.next_handle;
            state.next_handle += 1;
            state.file_handles.insert(
                fh,
                FileHandleState {
                    path: file.metadata.path.clone(),
                    content: file.content.into_bytes(),
                    dirty: false,
                },
            );
            (ino, fh)
        });

        reply.created(
            &TTL,
            &self.file_attr(ino, MemoryEntryKind::File, file.metadata.size, req),
            0,
            fh,
            0,
        );
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(parent_path) = self
            .rt
            .block_on(async { self.state.lock().await.path_for_ino(parent) })
        else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(path) = Self::join_child(&parent_path, name) else {
            reply.error(libc::EINVAL);
            return;
        };
        match self
            .rt
            .block_on(self.core.delete_file(&self.project_id, &path))
        {
            Ok(_) => {
                self.rt
                    .block_on(async { self.state.lock().await.remove_path(&path) });
                reply.ok();
            }
            Err(_) => reply.error(libc::ENOENT),
        }
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        let Some(parent_path) = self
            .rt
            .block_on(async { self.state.lock().await.path_for_ino(parent) })
        else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(newparent_path) = self
            .rt
            .block_on(async { self.state.lock().await.path_for_ino(newparent) })
        else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(from_path) = Self::join_child(&parent_path, name) else {
            reply.error(libc::EINVAL);
            return;
        };
        let Some(to_path) = Self::join_child(&newparent_path, newname) else {
            reply.error(libc::EINVAL);
            return;
        };

        match self.rt.block_on(self.core.rename_file(
            &self.project_id,
            &self.project_root,
            &from_path,
            &to_path,
        )) {
            Ok(resolved) => {
                self.rt.block_on(async {
                    let mut state = self.state.lock().await;
                    state.remove_path(&from_path);
                    state.inode_for_path(&resolved.logical_path);
                });
                reply.ok();
            }
            Err(_) => reply.error(libc::EIO),
        }
    }

    fn setattr(
        &mut self,
        req: &Request<'_>,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let Some(path) = self
            .rt
            .block_on(async { self.state.lock().await.path_for_ino(ino) })
        else {
            reply.error(libc::ENOENT);
            return;
        };

        if let Some(size) = size {
            let file = match self
                .rt
                .block_on(self.core.read_file(&self.project_id, &path))
            {
                Ok(file) => file,
                Err(_) => {
                    reply.error(libc::ENOENT);
                    return;
                }
            };
            let mut content = file.content.into_bytes();
            content.resize(size as usize, 0);
            match self.rt.block_on(self.core.write_file(
                &self.project_id,
                &self.project_root,
                &path,
                &String::from_utf8_lossy(&content),
            )) {
                Ok(updated) => {
                    reply.attr(
                        &TTL,
                        &self.file_attr(ino, MemoryEntryKind::File, updated.metadata.size, req),
                    );
                }
                Err(_) => reply.error(libc::EIO),
            }
            return;
        }

        match self.rt.block_on(self.core.stat(&self.project_id, &path)) {
            Ok(meta) => reply.attr(&TTL, &self.file_attr(ino, meta.kind, meta.size, req)),
            Err(_) => reply.error(libc::ENOENT),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers;
    use tokio_util::sync::CancellationToken;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disabled_mount_returns_none() {
        let state = AppState::new(test_helpers::create_test_db(), CancellationToken::new());
        let settings = djinn_core::models::DjinnSettings::default();
        assert!(
            resolve_mount_config(&state, &settings)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enabled_mount_requires_project_id() {
        let state = AppState::new(test_helpers::create_test_db(), CancellationToken::new());
        let settings = djinn_core::models::DjinnSettings {
            memory_mount_enabled: Some(true),
            ..Default::default()
        };
        let err = resolve_mount_config(&state, &settings).await.unwrap_err();
        if cfg!(all(
            target_os = "linux",
            feature = "linux-fuse-memory-mount"
        )) {
            assert!(matches!(err, MemoryMountConfigError::MissingProjectId));
        } else if cfg!(target_os = "linux") {
            assert!(matches!(err, MemoryMountConfigError::FeatureDisabled));
        } else {
            assert!(matches!(err, MemoryMountConfigError::NonLinux));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mount_path_must_be_absolute_when_overridden() {
        let db = test_helpers::create_test_db();
        let state = AppState::new(db.clone(), CancellationToken::new());
        let project = test_helpers::create_test_project_with_dir(&db).await.0;
        let settings = djinn_core::models::DjinnSettings {
            memory_mount_enabled: Some(true),
            memory_mount_project_id: Some(project.id),
            memory_mount_path: Some("relative/memory".to_string()),
            ..Default::default()
        };
        let err = resolve_mount_config(&state, &settings).await.unwrap_err();
        if cfg!(all(
            target_os = "linux",
            feature = "linux-fuse-memory-mount"
        )) {
            assert!(matches!(
                err,
                MemoryMountConfigError::MountPathNotAbsolute(_)
            ));
        } else if cfg!(target_os = "linux") {
            assert!(matches!(err, MemoryMountConfigError::FeatureDisabled));
        } else {
            assert!(matches!(err, MemoryMountConfigError::NonLinux));
        }
    }
}
