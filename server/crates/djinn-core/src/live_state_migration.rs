//! Shared serialization and durable filesystem publication primitives for
//! Release N live-state migrations.
//!
//! The lock is keyed by immutable project identity rather than a mutable path.
//! Filesystem publication deliberately never falls back to copy-and-delete:
//! callers must handle a cross-device layout before changing any source input.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum LiveStateMigrationError {
    #[error("live-state migration lock for project `{project_id}` is already held")]
    LockHeld { project_id: String },
    #[error("refusing to atomically rename across filesystems: {source_path} -> {destination}")]
    CrossFilesystem {
        source_path: PathBuf,
        destination: PathBuf,
    },
    #[error("refusing to operate on symlink `{0}`")]
    Symlink(PathBuf),
    #[error("{operation} `{path}`: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("injected failure at {0:?}")]
    InjectedFailure(AtomicWriteStep),
}

pub type Result<T> = std::result::Result<T, LiveStateMigrationError>;

/// Process-held exclusive lock for all migrations affecting one project.
///
/// Lock files live under the destination runtime parent so no old project-local
/// path is ever used for synchronization. The filename is a SHA-256 digest of
/// the immutable project id, avoiding path traversal and exposing no identity
/// in process-visible runtime paths.
pub struct ProjectLiveStateMigrationLock {
    #[allow(dead_code)]
    file: File,
    path: PathBuf,
}

impl ProjectLiveStateMigrationLock {
    pub fn try_acquire(destination_runtime_parent: &Path, project_id: &str) -> Result<Self> {
        let lock_dir = destination_runtime_parent.join(".live-state-migration-locks");
        fs::create_dir_all(&lock_dir).map_err(|source| LiveStateMigrationError::Io {
            operation: "create migration lock directory",
            path: lock_dir.clone(),
            source,
        })?;
        let digest = hex_digest(project_id.as_bytes());
        let path = lock_dir.join(format!("{digest}.lock"));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|source| LiveStateMigrationError::Io {
                operation: "open migration lock",
                path: path.clone(),
                source,
            })?;
        lock_exclusive_nonblocking(&file).map_err(|source| {
            if source.kind() == io::ErrorKind::WouldBlock {
                LiveStateMigrationError::LockHeld {
                    project_id: project_id.to_owned(),
                }
            } else {
                LiveStateMigrationError::Io {
                    operation: "acquire migration lock",
                    path: path.clone(),
                    source,
                }
            }
        })?;
        Ok(Self { file, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Failure injection points used to prove a failed configuration publication
/// leaves the previously published file intact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtomicWriteStep {
    BeforeWrite,
    AfterFileSync,
    BeforeRename,
    AfterRename,
    AfterDirectorySync,
}

/// Rename within a filesystem. This explicitly rejects a cross-device rename
/// rather than allowing platform-specific copy/delete behavior.
pub fn atomic_rename(source: &Path, destination: &Path) -> Result<()> {
    reject_symlink(source)?;
    if destination.symlink_metadata().is_ok() {
        reject_symlink(destination)?;
    }
    let source_device = device_for(source)?;
    let destination_parent = destination
        .parent()
        .ok_or_else(|| LiveStateMigrationError::Io {
            operation: "locate rename destination parent",
            path: destination.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "destination has no parent"),
        })?;
    let destination_device = device_for(destination_parent)?;
    if source_device != destination_device {
        return Err(LiveStateMigrationError::CrossFilesystem {
            source_path: source.to_path_buf(),
            destination: destination.to_path_buf(),
        });
    }
    fs::rename(source, destination).map_err(|source| LiveStateMigrationError::Io {
        operation: "atomic rename",
        path: destination.to_path_buf(),
        source,
    })
}

/// Publish configuration bytes through sibling-temp write, file sync, rename,
/// and parent-directory sync. `fail_at` is test-only behavior made explicit so
/// migration families can demonstrate source preservation without faulting the
/// process or monkey-patching filesystem calls.
pub fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    atomic_write_with_failure(path, contents, None)
}

pub fn atomic_write_with_failure(
    path: &Path,
    contents: &[u8],
    fail_at: Option<AtomicWriteStep>,
) -> Result<()> {
    let parent = path.parent().ok_or_else(|| LiveStateMigrationError::Io {
        operation: "locate configuration parent",
        path: path.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::InvalidInput,
            "configuration path has no parent",
        ),
    })?;
    let temp = sibling_temp_path(path);
    let result = (|| {
        fail_if(fail_at, AtomicWriteStep::BeforeWrite)?;
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)
            .map_err(|source| LiveStateMigrationError::Io {
                operation: "create configuration temporary file",
                path: temp.clone(),
                source,
            })?;
        file.write_all(contents)
            .map_err(|source| LiveStateMigrationError::Io {
                operation: "write configuration temporary file",
                path: temp.clone(),
                source,
            })?;
        file.sync_all()
            .map_err(|source| LiveStateMigrationError::Io {
                operation: "sync configuration temporary file",
                path: temp.clone(),
                source,
            })?;
        fail_if(fail_at, AtomicWriteStep::AfterFileSync)?;
        fail_if(fail_at, AtomicWriteStep::BeforeRename)?;
        atomic_rename(&temp, path)?;
        fail_if(fail_at, AtomicWriteStep::AfterRename)?;
        sync_directory(parent)?;
        fail_if(fail_at, AtomicWriteStep::AfterDirectorySync)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn fail_if(fail_at: Option<AtomicWriteStep>, step: AtomicWriteStep) -> Result<()> {
    if fail_at == Some(step) {
        Err(LiveStateMigrationError::InjectedFailure(step))
    } else {
        Ok(())
    }
}

fn sibling_temp_path(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(
        ".{name}.live-state-migration.tmp.{}",
        std::process::id()
    ))
}

fn reject_symlink(path: &Path) -> Result<()> {
    let metadata = path
        .symlink_metadata()
        .map_err(|source| LiveStateMigrationError::Io {
            operation: "classify path without following symlinks",
            path: path.to_path_buf(),
            source,
        })?;
    if metadata.file_type().is_symlink() {
        return Err(LiveStateMigrationError::Symlink(path.to_path_buf()));
    }
    Ok(())
}

#[cfg(unix)]
fn device_for(path: &Path) -> Result<u64> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path)
        .map(|metadata| metadata.dev())
        .map_err(|source| LiveStateMigrationError::Io {
            operation: "inspect filesystem device",
            path: path.to_path_buf(),
            source,
        })
}

#[cfg(not(unix))]
fn device_for(path: &Path) -> Result<u64> {
    // Windows rename is atomic only within a volume. The standard library does
    // not expose a stable volume id, so callers on that platform get a clear
    // unsupported error instead of a copy/delete fallback.
    Err(LiveStateMigrationError::Io {
        operation: "inspect filesystem device",
        path: path.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::Unsupported,
            "same-filesystem rename requires unix",
        ),
    })
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|source| LiveStateMigrationError::Io {
            operation: "sync configuration directory",
            path: path.to_path_buf(),
            source,
        })
}

#[cfg(unix)]
fn lock_exclusive_nonblocking(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: `file` remains open for the lock lifetime and flock only reads
    // its valid descriptor. LOCK_NB makes contention observable to callers.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn lock_exclusive_nonblocking(_file: &File) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "migration lock requires unix",
    ))
}

fn hex_digest(input: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_project_is_excluded_but_different_projects_can_proceed() {
        let temp = tempfile::tempdir().unwrap();
        let one = ProjectLiveStateMigrationLock::try_acquire(temp.path(), "project-a").unwrap();
        assert!(matches!(
            ProjectLiveStateMigrationLock::try_acquire(temp.path(), "project-a"),
            Err(LiveStateMigrationError::LockHeld { .. })
        ));
        let two = ProjectLiveStateMigrationLock::try_acquire(temp.path(), "project-b").unwrap();
        assert_ne!(one.path(), two.path());
    }

    #[test]
    fn injected_failure_preserves_existing_destination() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("settings.json");
        fs::write(&destination, b"old source").unwrap();
        assert!(matches!(
            atomic_write_with_failure(&destination, b"new", Some(AtomicWriteStep::BeforeRename)),
            Err(LiveStateMigrationError::InjectedFailure(
                AtomicWriteStep::BeforeRename
            ))
        ));
        assert_eq!(fs::read(&destination).unwrap(), b"old source");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cross_filesystem_rename_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::write(&source, b"source").unwrap();
        let shm = Path::new("/dev/shm");
        if shm.is_dir() && device_for(temp.path()).unwrap() != device_for(shm).unwrap() {
            assert!(matches!(
                atomic_rename(&source, &shm.join("djinn-live-state-migration-test")),
                Err(LiveStateMigrationError::CrossFilesystem { .. })
            ));
            assert_eq!(fs::read(&source).unwrap(), b"source");
        }
    }
}
