//! Output-only glob handling for the verification input fingerprint.
//!
//! `output_only_globs` do two separable jobs, and conflating them is what made
//! a warm, reusable final-verification plan impossible:
//!
//! 1. **Exclusion** — matching paths are left out of the fingerprint. This is
//!    unconditional and lives in `verification_input.rs`; without it, a warm
//!    worktree makes the fingerprint hash every byte of every ignored build
//!    artifact (`git ls-files --others -i` enumerates ignored files
//!    individually, and each one is then read in full).
//! 2. **Purge** — matching paths are deleted from disk before hashing. The
//!    attested tier requires it, because its strict launcher demands that
//!    output directories be absent at launch. The recorded tier must not do it,
//!    because the directory being excluded is the warm build cache the run
//!    depends on.
//!
//! [`OutputOnlyPolicy`] is the switch between the two. Note that the purge runs
//! on *every* fingerprint computation — including the C0/C1/C2 reuse
//! checkpoints, not only immediately before execution.

use std::path::Path;

use globset::GlobSet;

use crate::verification_input::{VerificationInputUnavailable, unavailable_manifest};

/// Whether output-only globs also purge the paths they exclude.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OutputOnlyPolicy {
    /// Delete every matching entry before hashing. Required by the attested
    /// tier, whose strict launcher rejects a pre-existing output directory.
    #[default]
    PurgeBeforeFingerprint,
    /// Exclude matching entries from the hash and leave them on disk. Required
    /// by the recorded tier, which reuses the warm outputs it excludes.
    ExcludeOnly,
}

pub(crate) fn cleanup_output_only(
    worktree: &Path,
    globs: &GlobSet,
) -> Result<(), VerificationInputUnavailable> {
    if globs.is_empty() {
        return Ok(());
    }
    let root = std::fs::canonicalize(worktree).map_err(|e| {
        VerificationInputUnavailable::UnreadableFile {
            path: ".".into(),
            error: e.to_string(),
        }
    })?;
    cleanup_output_dir(&root, &root, globs)
}

/// KNOWN LIMITATION (attested tier only). This deletes entries that MATCH the
/// globs. `server/target/**` compiles to `^server/target/.*$`, which matches the
/// children but not `server/target` itself, so an empty directory survives and
/// the strict launcher then rejects with `OutputOnlyPreexisting` — an attested
/// plan with output globs works at most once per worktree. No glob spelling
/// escapes it: a directory-matching glob has no literal prefix and so fails
/// `output_directories()`, while the pair `target` + `target/**` is rejected as
/// overlapping. Fixing it means changing what "absent at launch" means, which is
/// the attestation guarantee itself, so it is deliberately retained here rather
/// than papered over. The recorded tier never reaches this function.
fn cleanup_output_dir(
    root: &Path,
    dir: &Path,
    globs: &GlobSet,
) -> Result<(), VerificationInputUnavailable> {
    for entry in
        std::fs::read_dir(dir).map_err(|e| VerificationInputUnavailable::UnreadableFile {
            path: dir.display().to_string(),
            error: e.to_string(),
        })?
    {
        let entry = entry.map_err(|e| VerificationInputUnavailable::UnreadableFile {
            path: dir.display().to_string(),
            error: e.to_string(),
        })?;
        if entry.file_name() == ".git" {
            continue;
        }
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .map_err(|_| unavailable_manifest("output-only path escaped worktree"))?;
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|_| unavailable_manifest("output-only traversal changed"))?;
        if globs.is_match(rel) {
            if meta.file_type().is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            }
            .map_err(|e| VerificationInputUnavailable::UnreadableFile {
                path: rel.display().to_string(),
                error: e.to_string(),
            })?;
        } else if meta.file_type().is_dir() {
            cleanup_output_dir(root, &path, globs)?;
        }
    }
    Ok(())
}
