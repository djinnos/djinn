//! Content-addressable devcontainer hashing.
//!
//! The controller hashes `.devcontainer/devcontainer.json` and
//! `.devcontainer/devcontainer-lock.json` from the project's bare git
//! mirror at `HEAD`. The hash becomes the image tag suffix so a
//! kubelet-cached `IfNotPresent` never re-serves a stale build
//! (per plan §2: content-addressable tags).
//!
//! **Lockfile policy.** The spec-recommended workflow commits both files;
//! `devcontainer-lock.json` pins Feature SHAs for reproducibility. If the
//! lockfile is absent we still return a hash from `devcontainer.json`
//! alone — the warning is surfaced via the UI banner in PR 6, we don't
//! want to block the v1 build path over a best-practice signal.

use std::path::Path;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// The two paths we look up in `HEAD`. Extracted as constants so tests can
/// assert the exact paths without stringly-typed drift.
pub const DEVCONTAINER_PATH: &str = ".devcontainer/devcontainer.json";
pub const DEVCONTAINER_LOCK_PATH: &str = ".devcontainer/devcontainer-lock.json";

/// Hash the committed devcontainer spec at `HEAD` in the given bare git
/// mirror.
///
/// Returns:
/// - `Ok(Some(hex_sha256))` when `.devcontainer/devcontainer.json` exists
///   at `HEAD`. The lockfile, if present, is folded into the hash after
///   a length prefix separator so the two layouts are unambiguous.
/// - `Ok(None)` when the `devcontainer.json` is missing — projects without
///   a committed spec are skipped by the controller.
/// - `Err(..)` on unrecoverable git errors (repo can't be opened, HEAD
///   missing). Transient — the next mirror-fetch tick retries.
pub fn compute_devcontainer_hash(mirror_path: &Path) -> Result<Option<String>> {
    let repo = git2::Repository::open(mirror_path).with_context(|| {
        format!(
            "compute_devcontainer_hash: open bare mirror at {}",
            mirror_path.display()
        )
    })?;

    let head_tree = match repo.head() {
        Ok(head) => head.peel_to_tree().with_context(|| {
            format!(
                "compute_devcontainer_hash: peel HEAD to tree at {}",
                mirror_path.display()
            )
        })?,
        // Empty / no-HEAD repos: treat as "no devcontainer yet".
        Err(e) if e.code() == git2::ErrorCode::UnbornBranch => return Ok(None),
        Err(e) if e.code() == git2::ErrorCode::NotFound => return Ok(None),
        Err(e) => {
            return Err(anyhow::Error::new(e).context(format!(
                "compute_devcontainer_hash: read HEAD at {}",
                mirror_path.display()
            )));
        }
    };

    let Some(devcontainer_bytes) = read_blob(&repo, &head_tree, DEVCONTAINER_PATH)? else {
        return Ok(None);
    };
    let lock_bytes = read_blob(&repo, &head_tree, DEVCONTAINER_LOCK_PATH)?;

    let mut hasher = Sha256::new();
    // Length-prefix each blob so (a, b) and (a+b, empty) never collide.
    hasher.update((devcontainer_bytes.len() as u64).to_le_bytes());
    hasher.update(&devcontainer_bytes);
    match lock_bytes {
        Some(bytes) => {
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(&bytes);
        }
        None => {
            // Mark the "no lockfile" case in the hash so adding a lockfile
            // later flips the hash (and rebuilds). Zero-length sentinel.
            hasher.update(0u64.to_le_bytes());
        }
    }
    Ok(Some(hex::encode(hasher.finalize())))
}

/// Read the blob at `rel_path` under `tree`. Returns `Ok(None)` if the
/// path isn't present; `Err(..)` only on unrecoverable git errors.
fn read_blob(
    repo: &git2::Repository,
    tree: &git2::Tree<'_>,
    rel_path: &str,
) -> Result<Option<Vec<u8>>> {
    let entry = match tree.get_path(Path::new(rel_path)) {
        Ok(e) => e,
        Err(e) if e.code() == git2::ErrorCode::NotFound => return Ok(None),
        Err(e) => {
            return Err(anyhow::Error::new(e)
                .context(format!("tree.get_path({rel_path})")))
        }
    };
    let object = entry
        .to_object(repo)
        .with_context(|| format!("blob object for {rel_path}"))?;
    let blob = object
        .into_blob()
        .map_err(|_| anyhow::anyhow!("{rel_path}: expected blob"))?;
    Ok(Some(blob.content().to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Build a bare git repo containing the given tree of files at `HEAD`.
    fn bare_repo_with_files(files: &[(&str, &[u8])]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("tempdir");

        // Build the commit in a normal working repo, then clone `--bare`
        // so the test matches the production mirror layout.
        let work = tempfile::tempdir().expect("tempdir");
        let work_path = work.path();
        let repo = git2::Repository::init(work_path).expect("init working repo");
        {
            let mut cfg = repo.config().expect("config");
            cfg.set_str("user.name", "tester").unwrap();
            cfg.set_str("user.email", "tester@example.com").unwrap();
        }

        for (rel, bytes) in files {
            let full = work_path.join(rel);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&full, bytes).unwrap();
        }

        let mut index = repo.index().expect("index");
        for (rel, _) in files {
            index.add_path(Path::new(rel)).expect("add_path");
        }
        index.write().expect("write index");
        let oid = index.write_tree().expect("write_tree");
        let tree = repo.find_tree(oid).expect("find_tree");
        let sig = repo.signature().expect("signature");
        repo.commit(Some("HEAD"), &sig, &sig, "seed", &tree, &[])
            .expect("commit");

        // Clone as bare.
        let bare_path = tmp.path().join("mirror.git");
        git2::build::RepoBuilder::new()
            .bare(true)
            .clone(&format!("file://{}", work_path.display()), &bare_path)
            .expect("clone bare");

        tmp
    }

    #[test]
    fn returns_none_when_devcontainer_missing() {
        let tmp = bare_repo_with_files(&[("README.md", b"hi")]);
        let hash = compute_devcontainer_hash(&tmp.path().join("mirror.git")).unwrap();
        assert!(hash.is_none(), "no devcontainer -> no hash");
    }

    #[test]
    fn returns_hash_with_devcontainer_only() {
        let tmp = bare_repo_with_files(&[(DEVCONTAINER_PATH, b"{}")]);
        let hash = compute_devcontainer_hash(&tmp.path().join("mirror.git"))
            .unwrap()
            .expect("hash present");
        assert_eq!(hash.len(), 64, "sha256 hex length");
    }

    #[test]
    fn hash_is_deterministic_across_calls() {
        let tmp = bare_repo_with_files(&[
            (DEVCONTAINER_PATH, b"{\"name\": \"demo\"}"),
            (DEVCONTAINER_LOCK_PATH, b"{}"),
        ]);
        let path = tmp.path().join("mirror.git");
        let a = compute_devcontainer_hash(&path).unwrap().unwrap();
        let b = compute_devcontainer_hash(&path).unwrap().unwrap();
        assert_eq!(a, b, "deterministic");
    }

    #[test]
    fn adding_lockfile_flips_the_hash() {
        let without = bare_repo_with_files(&[(DEVCONTAINER_PATH, b"{}")]);
        let with = bare_repo_with_files(&[
            (DEVCONTAINER_PATH, b"{}"),
            (DEVCONTAINER_LOCK_PATH, b"{}"),
        ]);
        let a = compute_devcontainer_hash(&without.path().join("mirror.git"))
            .unwrap()
            .unwrap();
        let b = compute_devcontainer_hash(&with.path().join("mirror.git"))
            .unwrap()
            .unwrap();
        assert_ne!(
            a, b,
            "committing the lockfile must move the hash (rebuilds feature-pinned image)"
        );
    }

    #[test]
    fn modifying_devcontainer_flips_the_hash() {
        let a = bare_repo_with_files(&[(DEVCONTAINER_PATH, b"{\"name\":\"one\"}")]);
        let b = bare_repo_with_files(&[(DEVCONTAINER_PATH, b"{\"name\":\"two\"}")]);
        let ha = compute_devcontainer_hash(&a.path().join("mirror.git"))
            .unwrap()
            .unwrap();
        let hb = compute_devcontainer_hash(&b.path().join("mirror.git"))
            .unwrap()
            .unwrap();
        assert_ne!(ha, hb);
    }
}
