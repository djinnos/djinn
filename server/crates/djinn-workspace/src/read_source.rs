//! Disposable, owner-scoped read-source caches.
//!
//! Read sources are regenerated from their authoritative bare mirror below
//! `<owner root>/.task-runtime/read-sources/<project_id>`. Project-local
//! `.djinn` state is deliberately not inspected, moved, or used as an input.

use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Clone, Debug)]
pub struct ReadSourceRequest {
    pub owner_project_id: String,
    pub target_project_id: String,
    pub owner_root: PathBuf,
    pub mirror_path: PathBuf,
}

impl ReadSourceRequest {
    pub fn new(
        owner_project_id: String,
        target_project_id: String,
        owner_root: PathBuf,
        mirror_path: PathBuf,
    ) -> Self {
        Self {
            owner_project_id,
            target_project_id,
            owner_root,
            mirror_path,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadSourceResult {
    Existing(PathBuf),
    Materialized(PathBuf),
}

impl ReadSourceResult {
    pub fn path(&self) -> &Path {
        match self {
            Self::Existing(path) | Self::Materialized(path) => path,
        }
    }
}

#[derive(Debug, Error)]
pub enum ReadSourceError {
    #[error("task-local .djinn-read-sources is a retired path and cannot be materialized: {0}")]
    RetiredTaskLocalPath(PathBuf),
    #[error("read-source cache is not a clean detached checkout: {0}")]
    InvalidCache(PathBuf),
    #[error("mirror is not a readable git repository: {0}")]
    InvalidMirror(PathBuf),
    #[error("git: {0}")]
    Git(String),
    #[error("I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Materializes disposable, read-only owner caches from a bare mirror.
pub struct ReadSourceMaterializer;

impl ReadSourceMaterializer {
    pub fn destination_for(owner_root: &Path, target_project_id: &str) -> PathBuf {
        owner_root
            .join(".task-runtime/read-sources")
            .join(target_project_id)
    }

    /// Explicit negative guard for the retired task-local path. It is never a
    /// materialization source or destination.
    pub fn is_retired_task_local_path(path: &Path) -> bool {
        path.components()
            .any(|component| component.as_os_str() == ".djinn-read-sources")
    }

    pub fn ensure(request: ReadSourceRequest) -> Result<ReadSourceResult, ReadSourceError> {
        if Self::is_retired_task_local_path(&request.owner_root) {
            return Err(ReadSourceError::RetiredTaskLocalPath(request.owner_root));
        }
        let destination = Self::destination_for(&request.owner_root, &request.target_project_id);
        let expected = mirror_head(&request.mirror_path)?;
        match checkout_state(&destination, &expected)? {
            true => return Ok(ReadSourceResult::Existing(destination)),
            false if destination.exists() => {
                return Err(ReadSourceError::InvalidCache(destination));
            }
            false => {}
        }

        let parent = destination
            .parent()
            .expect("read-source destination has parent");
        fs::create_dir_all(parent).map_err(|source| ReadSourceError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        let staging = parent.join(format!(".{}.read-source", request.target_project_id));
        if staging.exists() {
            fs::remove_dir_all(&staging).map_err(|source| ReadSourceError::Io {
                path: staging.clone(),
                source,
            })?;
        }
        git(
            parent,
            &["clone", "--local", "--shared", "--no-checkout"],
            &[request.mirror_path.as_path(), staging.as_path()],
        )?;
        git(&staging, &["checkout", "--detach", "HEAD"], &[])?;
        if !checkout_state(&staging, &expected)? {
            let _ = fs::remove_dir_all(&staging);
            return Err(ReadSourceError::InvalidCache(staging));
        }
        fs::rename(&staging, &destination).map_err(|source| ReadSourceError::Io {
            path: destination.clone(),
            source,
        })?;
        Ok(ReadSourceResult::Materialized(destination))
    }
}

fn mirror_head(mirror: &Path) -> Result<String, ReadSourceError> {
    if !mirror.is_dir() {
        return Err(ReadSourceError::InvalidMirror(mirror.to_path_buf()));
    }
    git(mirror, &["rev-parse", "HEAD"], &[])
}

fn checkout_state(path: &Path, expected: &str) -> Result<bool, ReadSourceError> {
    if !path.exists() {
        return Ok(false);
    }
    if !path.is_dir() || !path.join(".git").exists() {
        return Ok(false);
    }
    let head = git(path, &["rev-parse", "HEAD"], &[])?;
    let detached = git(path, &["symbolic-ref", "-q", "HEAD"], &[]).is_err();
    let clean = git(path, &["status", "--porcelain"], &[])?.is_empty();
    Ok(detached && clean && head == expected)
}

fn git(cwd: &Path, args: &[&str], paths: &[&Path]) -> Result<String, ReadSourceError> {
    let mut command: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    command.extend(paths.iter().map(|path| path.to_string_lossy().into_owned()));
    let output = djinn_git::run_git_command_binary_in(cwd, command)
        .map_err(|error| ReadSourceError::Git(error.to_string()))?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn run_git(dir: &Path, args: &[&str]) {
        git(dir, args, &[]).unwrap();
    }

    #[test]
    fn absent_cache_regenerates_from_real_mirror_without_consuming_residue() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        run_git(&source, &["init", "-b", "main"]);
        run_git(&source, &["config", "user.email", "test@example.com"]);
        run_git(&source, &["config", "user.name", "Test"]);
        fs::write(source.join("README.md"), "authoritative\n").unwrap();
        run_git(&source, &["add", "."]);
        run_git(&source, &["commit", "-m", "initial"]);
        let mirror = temp.path().join("mirror.git");
        git(
            temp.path(),
            &["clone", "--bare"],
            &[source.as_path(), mirror.as_path()],
        )
        .unwrap();
        let owner = temp.path().join("owner");
        let retired = owner.join(".djinn/read-sources/target");
        fs::create_dir_all(&retired).unwrap();
        fs::write(retired.join("sentinel"), "never consumed").unwrap();
        let result = ReadSourceMaterializer::ensure(ReadSourceRequest::new(
            "owner".into(),
            "target".into(),
            owner.clone(),
            mirror.clone(),
        ))
        .unwrap();
        let destination = ReadSourceMaterializer::destination_for(&owner, "target");
        assert_eq!(result.path(), destination);
        assert_eq!(
            fs::read_to_string(destination.join("README.md")).unwrap(),
            "authoritative\n"
        );
        assert_eq!(
            git(&destination, &["rev-parse", "HEAD"], &[]).unwrap(),
            mirror_head(&mirror).unwrap()
        );
        assert_eq!(
            fs::read_to_string(retired.join("sentinel")).unwrap(),
            "never consumed"
        );
    }

    #[test]
    fn task_local_retired_path_is_hard_denied() {
        assert!(ReadSourceMaterializer::is_retired_task_local_path(
            Path::new("workspace/.djinn-read-sources/target")
        ));
    }
}
