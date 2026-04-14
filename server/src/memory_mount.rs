//! Linux-only ADR-057 wave-1 memory mount plumbing.
//!
//! This module wires the transport-neutral [`crate::memory_fs::MemoryFilesystemCore`] into an
//! initial FUSE adapter behind the `memory-mount` cargo feature and typed server settings:
//!
//! - `memory_mount_enabled: true`
//! - `memory_mount_path: "/absolute/mount/path"`
//!
//! The mount is disabled by default. When enabled on Linux with the cargo feature present, Djinn
//! mounts the single registered project's memory note tree at `memory_mount_path` using FUSE.
//!
//! Operational safety for this wave:
//! - startup rejects invalid configuration before the HTTP server begins serving
//! - the mountpoint must already exist, be absolute, and be empty
//! - only Linux + `--features memory-mount` is supported in this slice
//! - exactly one registered project is supported; branch-aware and multi-project mounting remain
//!   out of scope for later ADR-057 waves

use std::collections::HashMap;
use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use djinn_core::models::DjinnSettings;
use djinn_db::{NoteRepository, ProjectRepository};

use crate::events::EventBus;
use crate::memory_fs::{MemoryEntryKind, MemoryFilesystemCore};

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
    #[cfg(all(target_os = "linux", feature = "memory-mount"))]
    session: Option<fuser::BackgroundSession>,
}

impl MountedMemoryFilesystem {
    pub fn disabled() -> Self {
        Self {
            #[cfg(all(target_os = "linux", feature = "memory-mount"))]
            session: None,
        }
    }

    #[cfg(all(target_os = "linux", feature = "memory-mount"))]
    fn from_session(session: fuser::BackgroundSession) -> Self {
        Self {
            session: Some(session),
        }
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
        bail!("memory mount is only supported on Linux in ADR-057 wave 1")
    }

    #[cfg(target_os = "linux")]
    {
        if !cfg!(feature = "memory-mount") {
            bail!(
                "memory mount is enabled in settings but djinn-server was built without the `memory-mount` feature"
            );
        }

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
    db: djinn_db::Database,
    events: EventBus,
) -> Result<Option<MountedMemoryFilesystem>> {
    let Some(resolved) = validate_mount_config(settings, db.clone(), events.clone()).await? else {
        return Ok(None);
    };

    #[cfg(all(target_os = "linux", feature = "memory-mount"))]
    {
        let repo = NoteRepository::new(db, events);
        let fs = LinuxMemoryFilesystem::new(repo, resolved.project_id, resolved.project_path);
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
        tracing::info!(mount_path = %resolved.mount_path.display(), "memory filesystem mounted");
        Ok(Some(MountedMemoryFilesystem::from_session(session)))
    }

    #[cfg(not(all(target_os = "linux", feature = "memory-mount")))]
    {
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
    core: MemoryFilesystemCore,
    project_id: String,
    project_path: PathBuf,
    runtime: tokio::runtime::Handle,
    file_handles: Arc<Mutex<HashMap<u64, String>>>,
    next_handle: std::sync::atomic::AtomicU64,
}

#[cfg(all(target_os = "linux", feature = "memory-mount"))]
impl LinuxMemoryFilesystem {
    fn new(repo: NoteRepository, project_id: String, project_path: PathBuf) -> Self {
        Self {
            core: MemoryFilesystemCore::new(repo),
            project_id,
            project_path,
            runtime: tokio::runtime::Handle::current(),
            file_handles: Arc::new(Mutex::new(HashMap::new())),
            next_handle: std::sync::atomic::AtomicU64::new(1),
        }
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
        fn visit(fs: &LinuxMemoryFilesystem, path: &str, entries: &mut Vec<String>) -> Result<()> {
            entries.push(path.to_string());
            let metadata = fs
                .runtime
                .block_on(fs.core.stat(&fs.project_id, path))
                .map_err(|e| anyhow!(e.to_string()))?;
            if metadata.kind != MemoryEntryKind::Directory {
                return Ok(());
            }
            let children = fs
                .runtime
                .block_on(fs.core.list_dir(&fs.project_id, path))
                .map_err(|e| anyhow!(e.to_string()))?;
            for child in children {
                visit(fs, &child.metadata.path, entries)?;
            }
            Ok(())
        }

        let mut entries = Vec::new();
        visit(self, "", &mut entries)?;
        Ok(entries)
    }

    fn attr_for_path(&self, path: &str) -> Result<fuser::FileAttr, i32> {
        let metadata = self
            .runtime
            .block_on(self.core.stat(&self.project_id, path))
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
        self.release_handle(fh);
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
        let file = match self
            .runtime
            .block_on(self.core.read_file(&self.project_id, &path))
        {
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
        let existing = self
            .runtime
            .block_on(self.core.read_file(&self.project_id, &path))
            .map(|file| file.content)
            .unwrap_or_default();

        let mut bytes = existing.into_bytes();
        let offset = offset.max(0) as usize;
        if bytes.len() < offset {
            bytes.resize(offset, 0);
        }
        if bytes.len() < offset + data.len() {
            bytes.resize(offset + data.len(), 0);
        }
        bytes[offset..offset + data.len()].copy_from_slice(data);
        let content = match String::from_utf8(bytes) {
            Ok(content) => content,
            Err(_) => return reply.error(libc::EINVAL),
        };

        match self.runtime.block_on(self.core.write_file(
            &self.project_id,
            &self.project_path,
            &path,
            &content,
        )) {
            Ok(_) => reply.written(data.len() as u32),
            Err(err) => reply.error(repo_err_to_errno(err)),
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
        let existing = match self
            .runtime
            .block_on(self.core.read_file(&self.project_id, &path))
        {
            Ok(file) => file.content,
            Err(err) => return reply.error(repo_err_to_errno(err)),
        };
        let mut bytes = existing.into_bytes();
        bytes.resize(size as usize, 0);
        let content = match String::from_utf8(bytes) {
            Ok(content) => content,
            Err(_) => return reply.error(libc::EINVAL),
        };
        match self.runtime.block_on(self.core.write_file(
            &self.project_id,
            &self.project_path,
            &path,
            &content,
        )) {
            Ok(file) => reply.attr(&TTL, &file_attr_for_metadata(&file.metadata)),
            Err(err) => reply.error(repo_err_to_errno(err)),
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
        let children = match self
            .runtime
            .block_on(self.core.list_dir(&self.project_id, &path))
        {
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
        match self.runtime.block_on(self.core.write_file(
            &self.project_id,
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
        match self
            .runtime
            .block_on(self.core.delete_file(&self.project_id, &path))
        {
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
        match self.runtime.block_on(self.core.rename_file(
            &self.project_id,
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
    use crate::test_helpers::{create_test_db, test_events};

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
        assert!(error.to_string().contains("absolute path"));
    }
}
