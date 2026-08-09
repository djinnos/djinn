//! Deterministic local construction of one direct-delivery commit.
//!
//! This module deliberately writes only Git objects.  In particular, it never
//! moves a ref, invokes a remote, or uses the repository's normal index.  The
//! caller supplies every value that influences the commit object.

use std::{path::Path, sync::atomic::{AtomicU64, Ordering}};

use djinn_core::models::TaskDeliveryIdentity;
use sha2::{Digest, Sha256};

use crate::{GitError, run_git_command_in_with_env, run_git_command_in_with_env_allow_failure};

static SCRATCH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Explicit Git identity and timestamp for a generated delivery commit.
///
/// `when` is the Git external date form, for example `"1700000000 +0000"`.
/// It is persisted preparation data, rather than a time observed while a
/// candidate is being built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectDeliverySignature {
    pub name: String,
    pub email: String,
    pub when: String,
}

/// Immutable inputs for a direct-delivery candidate.
///
/// `normalized_patch` is the source patch after the preparation layer's source
/// normalization.  This builder normalizes line endings again defensively and
/// returns the digest of those exact bytes.  `source_sha` is included in the
/// commit message, so a reworked source identity cannot reuse a candidate even
/// where two source revisions happen to produce identical patches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectDeliveryInput {
    pub identity: TaskDeliveryIdentity,
    pub selected_parent_sha: String,
    pub source_sha: String,
    pub normalized_patch: String,
    pub author: DirectDeliverySignature,
    pub committer: DirectDeliverySignature,
    pub message: String,
}

/// Successfully built, locally inspectable candidate data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectDeliveryCandidate {
    pub candidate_sha: String,
    pub normalized_patch_digest: String,
    pub tree_sha: String,
    pub first_parent_sha: String,
}

/// A non-mutating candidate construction outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirectDeliveryBuild {
    Clean(DirectDeliveryCandidate),
    Conflict { normalized_patch_digest: String, reason: String },
    InvalidSource { reason: String },
}

/// Construct one commit with exactly `selected_parent_sha` as its only parent.
///
/// The temporary index is isolated through `GIT_INDEX_FILE`; therefore a
/// conflict cannot alter the selected attempt ref, worktree index, or a remote
/// ref.  The resulting object is intentionally left in the local object store
/// for later inspection/publishing by the orchestration layer.
pub async fn build_direct_delivery_candidate(
    repository: impl AsRef<Path>,
    input: &DirectDeliveryInput,
) -> Result<DirectDeliveryBuild, GitError> {
    let repository = repository.as_ref();
    let normalized_patch = normalize_patch(&input.normalized_patch);
    let normalized_patch_digest = sha256_hex(&normalized_patch);

    if let Err(reason) = validate_input(input, &normalized_patch) {
        return Ok(DirectDeliveryBuild::InvalidSource { reason });
    }

    for (label, sha) in [("selected_parent_sha", &input.selected_parent_sha)] {
        let outcome = run_git_command_in_with_env_allow_failure(
            repository,
            vec!["rev-parse".into(), "--verify".into(), format!("{sha}^{{commit}}")],
            Vec::new(),
        )
        .await?;
        if !outcome.is_success() || outcome.stdout.trim() != sha {
            return Ok(DirectDeliveryBuild::InvalidSource {
                reason: format!("{label} is not an exact local commit SHA"),
            });
        }
    }

    let scratch = ScratchFiles::new()?;
    std::fs::write(&scratch.patch, &normalized_patch)?;
    std::fs::write(&scratch.message, commit_message(input, &normalized_patch_digest))?;
    let index_env = vec![("GIT_INDEX_FILE".into(), scratch.index.display().to_string())];

    run_git_command_in_with_env(
        repository,
        vec!["read-tree".into(), input.selected_parent_sha.clone()],
        index_env.clone(),
    )
    .await?;

    let apply = run_git_command_in_with_env_allow_failure(
        repository,
        vec![
            "apply".into(),
            "--cached".into(),
            "--whitespace=nowarn".into(),
            scratch.patch.display().to_string(),
        ],
        index_env.clone(),
    )
    .await?;
    if !apply.is_success() {
        let reason = command_reason(&apply.stderr, &apply.stdout);
        return Ok(if looks_like_invalid_patch(&reason) {
            DirectDeliveryBuild::InvalidSource { reason }
        } else {
            DirectDeliveryBuild::Conflict {
                normalized_patch_digest,
                reason,
            }
        });
    }

    let tree_sha = run_git_command_in_with_env(
        repository,
        vec!["write-tree".into()],
        index_env,
    )
    .await?
    .stdout
    .trim()
    .to_string();
    let mut commit_env = signature_env("AUTHOR", &input.author);
    commit_env.extend(signature_env("COMMITTER", &input.committer));
    let candidate_sha = run_git_command_in_with_env(
        repository,
        vec![
            "commit-tree".into(),
            tree_sha.clone(),
            "-p".into(),
            input.selected_parent_sha.clone(),
            "-F".into(),
            scratch.message.display().to_string(),
        ],
        commit_env,
    )
    .await?
    .stdout
    .trim()
    .to_string();

    Ok(DirectDeliveryBuild::Clean(DirectDeliveryCandidate {
        candidate_sha,
        normalized_patch_digest,
        tree_sha,
        first_parent_sha: input.selected_parent_sha.clone(),
    }))
}

fn validate_input(input: &DirectDeliveryInput, normalized_patch: &str) -> Result<(), String> {
    input.identity.validate().map_err(|error| error.to_string())?;
    for (label, value) in [
        ("selected_parent_sha", input.selected_parent_sha.as_str()),
        ("source_sha", input.source_sha.as_str()),
        ("message", input.message.as_str()),
        ("normalized_patch", normalized_patch),
        ("author.name", input.author.name.as_str()),
        ("author.email", input.author.email.as_str()),
        ("author.when", input.author.when.as_str()),
        ("committer.name", input.committer.name.as_str()),
        ("committer.email", input.committer.email.as_str()),
        ("committer.when", input.committer.when.as_str()),
    ] {
        if value.trim().is_empty() || value.contains('\0') {
            return Err(format!("{label} must be nonblank and contain no NUL"));
        }
    }
    if !is_hex_sha(&input.selected_parent_sha) || !is_hex_sha(&input.source_sha) {
        return Err("selected_parent_sha and source_sha must be 40 or 64 lowercase hex characters".into());
    }
    Ok(())
}

fn is_hex_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn normalize_patch(patch: &str) -> String {
    let patch = patch.replace("\r\n", "\n").replace('\r', "\n");
    if patch.ends_with('\n') { patch } else { format!("{patch}\n") }
}

/// Produce the complete, deterministic commit message.
///
/// The normalized patch digest is deliberately part of the commit object rather
/// than only returned as inspection data. Two distinct valid patch encodings
/// may produce the same tree; binding their digest here keeps their candidates
/// distinct for ledger identity and replay.
fn commit_message(input: &DirectDeliveryInput, normalized_patch_digest: &str) -> String {
    format!(
        "{}\n\nDjinn-Delivery-Attempt: {}\nDjinn-Delivery-Task: {}\nDjinn-Delivery-Generation: {}\nDjinn-Source: {}\nDjinn-Normalized-Patch-Digest: {}\n",
        input.message.trim_end(),
        input.identity.build_attempt_id,
        input.identity.task_id,
        input.identity.delivery_generation,
        input.source_sha,
        normalized_patch_digest,
    )
}

fn signature_env(prefix: &str, signature: &DirectDeliverySignature) -> Vec<(String, String)> {
    vec![
        (format!("GIT_{prefix}_NAME"), signature.name.clone()),
        (format!("GIT_{prefix}_EMAIL"), signature.email.clone()),
        (format!("GIT_{prefix}_DATE"), signature.when.clone()),
    ]
}

fn sha256_hex(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn command_reason(stderr: &str, stdout: &str) -> String {
    let text = if stderr.trim().is_empty() { stdout } else { stderr };
    text.trim().to_string()
}

fn looks_like_invalid_patch(reason: &str) -> bool {
    let reason = reason.to_ascii_lowercase();
    ["corrupt patch", "unrecognized input", "no valid patches", "patch fragment without header"]
        .iter()
        .any(|needle| reason.contains(needle))
}

struct ScratchFiles {
    index: std::path::PathBuf,
    patch: std::path::PathBuf,
    message: std::path::PathBuf,
}

impl ScratchFiles {
    fn new() -> Result<Self, std::io::Error> {
        let sequence = SCRATCH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("djinn-direct-delivery-{}-{sequence}", std::process::id()));
        std::fs::create_dir_all(&root)?;
        Ok(Self { index: root.join("index"), patch: root.join("patch"), message: root.join("message") })
    }
}

impl Drop for ScratchFiles {
    fn drop(&mut self) {
        if let Some(root) = self.index.parent() {
            let _ = std::fs::remove_dir_all(root);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{git, init_repo_with_main_commit, write_and_commit};

    fn sha(repo: &Path, revision: &str) -> String {
        String::from_utf8(git(repo, ["rev-parse", revision]).stdout).unwrap().trim().into()
    }

    fn input(parent: String, source: String) -> DirectDeliveryInput {
        DirectDeliveryInput {
            identity: TaskDeliveryIdentity::new("attempt-1", "task-1", 1).unwrap(),
            selected_parent_sha: parent,
            source_sha: source,
            normalized_patch: "diff --git a/README.md b/README.md\nindex ce01362..94954ab 100644\n--- a/README.md\n+++ b/README.md\n@@ -1 +1 @@\n-hello\n+delivered\n".into(),
            author: DirectDeliverySignature { name: "Djinn".into(), email: "djinn@example.test".into(), when: "1700000000 +0000".into() },
            committer: DirectDeliverySignature { name: "Djinn".into(), email: "djinn@example.test".into(), when: "1700000000 +0000".into() },
            message: "Deliver task".into(),
        }
    }

    async fn clean(repo: &Path, input: &DirectDeliveryInput) -> DirectDeliveryCandidate {
        match build_direct_delivery_candidate(repo, input).await.unwrap() {
            DirectDeliveryBuild::Clean(candidate) => candidate,
            other => panic!("expected clean result, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn replay_is_deterministic_and_has_one_selected_first_parent() {
        let first = init_repo_with_main_commit();
        let parent = sha(first.path(), "main");
        let source = parent.clone();
        let candidate = clean(first.path(), &input(parent.clone(), source.clone())).await;
        let second = tempfile::tempdir().unwrap();
        git(
            second.path(),
            [
                "clone",
                "--no-local",
                first.path().to_str().unwrap(),
                ".",
            ],
        );
        let replay = clean(second.path(), &input(parent.clone(), source)).await;
        assert_eq!(candidate.candidate_sha, replay.candidate_sha);
        assert_eq!(candidate.normalized_patch_digest, replay.normalized_patch_digest);
        assert_eq!(sha(first.path(), &format!("{}^", candidate.candidate_sha)), parent);
        assert_eq!(git(first.path(), ["rev-list", "--count", &format!("{}^..{}", candidate.candidate_sha, candidate.candidate_sha)]).stdout, b"1\n");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parent_generation_source_and_normalized_patch_change_candidate() {
        let fixture = init_repo_with_main_commit();
        let parent = sha(fixture.path(), "main");
        let base = input(parent.clone(), parent.clone());
        let first = clean(fixture.path(), &base).await;
        write_and_commit(fixture.path(), "other.txt", "other\n", "other");
        let changed_parent = clean(fixture.path(), &input(sha(fixture.path(), "HEAD"), parent.clone())).await;
        let mut changed_generation = base.clone(); changed_generation.identity.delivery_generation = 2;
        let mut changed_source = base.clone(); changed_source.source_sha = sha(fixture.path(), "HEAD");
        let mut changed_patch = base.clone(); changed_patch.normalized_patch = base.normalized_patch.replace("delivered", "altered");
        assert_ne!(first.candidate_sha, changed_parent.candidate_sha);
        assert_ne!(first.candidate_sha, clean(fixture.path(), &changed_generation).await.candidate_sha);
        assert_ne!(first.candidate_sha, clean(fixture.path(), &changed_source).await.candidate_sha);
        assert_ne!(first.candidate_sha, clean(fixture.path(), &changed_patch).await.candidate_sha);
        assert_ne!(first.normalized_patch_digest, clean(fixture.path(), &changed_patch).await.normalized_patch_digest);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn normalized_patch_encoding_changes_candidate_when_tree_is_unchanged() {
        let fixture = init_repo_with_main_commit();
        let parent = sha(fixture.path(), "main");
        let with_index_metadata = input(parent.clone(), parent);
        let mut without_index_metadata = with_index_metadata.clone();
        without_index_metadata.normalized_patch = without_index_metadata
            .normalized_patch
            .replace("index ce01362..94954ab 100644\n", "");

        let indexed_candidate = clean(fixture.path(), &with_index_metadata).await;
        let metadata_free_candidate = clean(fixture.path(), &without_index_metadata).await;

        assert_eq!(indexed_candidate.tree_sha, metadata_free_candidate.tree_sha);
        assert_ne!(indexed_candidate.normalized_patch_digest, metadata_free_candidate.normalized_patch_digest);
        assert_ne!(indexed_candidate.candidate_sha, metadata_free_candidate.candidate_sha);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn conflict_does_not_move_attempt_ref_or_publish_remote_ref() {
        let fixture = init_repo_with_main_commit();
        let parent = sha(fixture.path(), "main");
        let before_ref = parent.clone();
        let mut conflicting = input(parent.clone(), parent);
        conflicting.normalized_patch = conflicting.normalized_patch.replace("-hello", "-not-present");
        let result = build_direct_delivery_candidate(fixture.path(), &conflicting).await.unwrap();
        assert!(matches!(result, DirectDeliveryBuild::Conflict { .. }));
        assert_eq!(sha(fixture.path(), "main"), before_ref);
        assert!(git(fixture.path(), ["remote"]).stdout.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn malformed_source_is_typed_invalid_source() {
        let fixture = init_repo_with_main_commit();
        let mut malformed = input(sha(fixture.path(), "main"), sha(fixture.path(), "main"));
        malformed.source_sha = "not-a-sha".into();
        assert!(matches!(build_direct_delivery_candidate(fixture.path(), &malformed).await.unwrap(), DirectDeliveryBuild::InvalidSource { .. }));
    }
}
