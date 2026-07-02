use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::{GitError, run_git_command};

/// Default base ref used when fingerprinting task submissions.
///
/// Task worktrees normally branch from the project's target branch, which
/// defaults to `main` when no project-specific target branch is configured.
pub const DEFAULT_SUBMISSION_BASE_REF: &str = "main";

/// Configuration for computing a complete task-submission diff fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmissionDiffFingerprintConfig {
    /// Base branch/ref used to find committed task-branch changes via
    /// `git merge-base <base_ref> HEAD`.
    pub base_ref: String,
}

impl Default for SubmissionDiffFingerprintConfig {
    fn default() -> Self {
        Self {
            base_ref: DEFAULT_SUBMISSION_BASE_REF.to_string(),
        }
    }
}

impl SubmissionDiffFingerprintConfig {
    pub fn new(base_ref: impl Into<String>) -> Self {
        Self {
            base_ref: base_ref.into(),
        }
    }
}

/// Result of computing a complete task-submission diff fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmissionDiffFingerprint {
    /// The task submission has no committed, staged, dirty tracked, or
    /// untracked text-file delta relative to the configured base.
    NoDiff(SubmissionNoDiff),
    /// The task submission has a canonical delta and therefore a stable digest.
    Diff(SubmissionDiffDigest),
}

impl SubmissionDiffFingerprint {
    pub fn is_no_diff(&self) -> bool {
        matches!(self, Self::NoDiff(_))
    }

    pub fn fingerprint(&self) -> Option<&str> {
        match self {
            Self::NoDiff(_) => None,
            Self::Diff(diff) => Some(&diff.fingerprint),
        }
    }

    pub fn canonical_diff_len(&self) -> usize {
        match self {
            Self::NoDiff(no_diff) => no_diff.canonical_diff_len,
            Self::Diff(diff) => diff.canonical_diff_len,
        }
    }

    pub fn changed_paths(&self) -> &[String] {
        match self {
            Self::NoDiff(no_diff) => &no_diff.changed_paths,
            Self::Diff(diff) => &diff.changed_paths,
        }
    }
}

/// Explicit no-diff result. This intentionally carries no fingerprint so
/// rejected-submission comparison code cannot fabricate historical state for an
/// empty submit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmissionNoDiff {
    pub base_ref: String,
    pub merge_base: Option<String>,
    pub canonical_diff_len: usize,
    pub changed_paths: Vec<String>,
}

/// Stable digest and diagnostics for a non-empty submission delta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmissionDiffDigest {
    pub fingerprint: String,
    pub base_ref: String,
    pub merge_base: Option<String>,
    pub canonical_diff_len: usize,
    pub changed_paths: Vec<String>,
}

/// Compute a complete task-submission diff fingerprint using the default base
/// ref (`main`).
pub async fn compute_submission_diff_fingerprint(
    worktree: impl AsRef<Path>,
) -> Result<SubmissionDiffFingerprint, GitError> {
    compute_submission_diff_fingerprint_with_config(
        worktree,
        &SubmissionDiffFingerprintConfig::default(),
    )
    .await
}

/// Compute a complete task-submission diff fingerprint.
///
/// The canonical input includes, in deterministic sections:
/// - committed task-branch changes from `merge-base(base_ref, HEAD)..HEAD`;
/// - staged/index changes (`git diff --cached HEAD`);
/// - dirty tracked worktree changes (`git diff`);
/// - untracked UTF-8 text files from `git ls-files --others --exclude-standard`.
pub async fn compute_submission_diff_fingerprint_with_config(
    worktree: impl AsRef<Path>,
    config: &SubmissionDiffFingerprintConfig,
) -> Result<SubmissionDiffFingerprint, GitError> {
    let worktree = worktree.as_ref();
    let base_ref = config.base_ref.trim();
    let base_ref = if base_ref.is_empty() {
        DEFAULT_SUBMISSION_BASE_REF
    } else {
        base_ref
    };
    let resolved_base_ref = resolve_base_ref(worktree, base_ref).await?;
    let merge_base = git_stdout(
        worktree,
        vec![
            "merge-base".into(),
            resolved_base_ref.clone(),
            "HEAD".into(),
        ],
    )
    .await?
    .trim()
    .to_string();

    let committed_diff = git_stdout(
        worktree,
        vec![
            "diff".into(),
            "--no-ext-diff".into(),
            "--no-renames".into(),
            merge_base.clone(),
            "HEAD".into(),
        ],
    )
    .await?;
    let staged_diff = git_stdout(
        worktree,
        vec![
            "diff".into(),
            "--cached".into(),
            "--no-ext-diff".into(),
            "--no-renames".into(),
            "HEAD".into(),
        ],
    )
    .await?;
    let dirty_tracked_diff = git_stdout(
        worktree,
        vec!["diff".into(), "--no-ext-diff".into(), "--no-renames".into()],
    )
    .await?;
    let untracked_text = canonical_untracked_text(worktree).await?;

    let canonical = canonical_submission_diff(
        &merge_base,
        &committed_diff,
        &staged_diff,
        &dirty_tracked_diff,
        &untracked_text.text,
    );
    let changed_paths = changed_path_summary(worktree, &merge_base, &untracked_text.paths).await?;
    let canonical_diff_len = canonical.len();

    if canonical.is_empty() {
        return Ok(SubmissionDiffFingerprint::NoDiff(SubmissionNoDiff {
            base_ref: resolved_base_ref,
            merge_base: Some(merge_base),
            canonical_diff_len,
            changed_paths,
        }));
    }

    Ok(SubmissionDiffFingerprint::Diff(SubmissionDiffDigest {
        fingerprint: sha256_hex(&canonical),
        base_ref: resolved_base_ref,
        merge_base: Some(merge_base),
        canonical_diff_len,
        changed_paths,
    }))
}

fn canonical_submission_diff(
    merge_base: &str,
    committed_diff: &str,
    staged_diff: &str,
    dirty_tracked_diff: &str,
    untracked_text: &str,
) -> String {
    let sections = [
        ("committed", committed_diff),
        ("staged", staged_diff),
        ("dirty_tracked", dirty_tracked_diff),
        ("untracked_text", untracked_text),
    ];
    let mut canonical = String::new();
    for (name, text) in sections {
        if text.is_empty() {
            continue;
        }
        canonical.push_str("djinn-submission-diff-section ");
        canonical.push_str(name);
        canonical.push('\n');
        if name == "committed" {
            canonical.push_str("merge-base ");
            canonical.push_str(merge_base);
            canonical.push('\n');
        }
        canonical.push_str(text);
        if !text.ends_with('\n') {
            canonical.push('\n');
        }
    }
    canonical
}

async fn resolve_base_ref(worktree: &Path, base_ref: &str) -> Result<String, GitError> {
    if rev_parse_verify(worktree, base_ref).await.is_ok() {
        return Ok(base_ref.to_string());
    }
    let origin_ref = format!("origin/{base_ref}");
    rev_parse_verify(worktree, &origin_ref).await?;
    Ok(origin_ref)
}

async fn rev_parse_verify(worktree: &Path, ref_name: &str) -> Result<String, GitError> {
    git_stdout(
        worktree,
        vec!["rev-parse".into(), "--verify".into(), ref_name.into()],
    )
    .await
}

async fn changed_path_summary(
    worktree: &Path,
    merge_base: &str,
    untracked_text_paths: &[String],
) -> Result<Vec<String>, GitError> {
    let mut paths = Vec::new();
    paths.extend(diff_name_only(worktree, vec![merge_base.into(), "HEAD".into()]).await?);
    paths.extend(diff_name_only(worktree, vec!["--cached".into(), "HEAD".into()]).await?);
    paths.extend(diff_name_only(worktree, Vec::new()).await?);
    paths.extend(untracked_text_paths.iter().cloned());
    paths.sort();
    paths.dedup();
    Ok(paths)
}

async fn diff_name_only(worktree: &Path, extra_args: Vec<String>) -> Result<Vec<String>, GitError> {
    let mut args = vec![
        "diff".to_string(),
        "--name-only".to_string(),
        "-z".to_string(),
        "--no-renames".to_string(),
    ];
    args.extend(extra_args);
    let output = git_stdout(worktree, args).await?;
    Ok(split_nul_paths(&output))
}

struct UntrackedText {
    text: String,
    paths: Vec<String>,
}

async fn canonical_untracked_text(worktree: &Path) -> Result<UntrackedText, GitError> {
    let output = git_stdout(
        worktree,
        vec![
            "ls-files".into(),
            "--others".into(),
            "--exclude-standard".into(),
            "-z".into(),
        ],
    )
    .await?;
    let mut paths = split_nul_paths(&output);
    paths.sort();

    let mut included_paths = Vec::new();
    let mut text = String::new();
    for path in paths {
        let full_path = worktree.join(&path);
        let Ok(metadata) = std::fs::symlink_metadata(&full_path) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let Ok(bytes) = std::fs::read(&full_path) else {
            continue;
        };
        if bytes.contains(&0) {
            continue;
        }
        let Ok(content) = String::from_utf8(bytes) else {
            continue;
        };
        included_paths.push(path.clone());
        text.push_str("--- /dev/null\n+++ b/");
        text.push_str(&path);
        text.push('\n');
        text.push_str("@@ untracked text file ");
        text.push_str(&content.len().to_string());
        text.push_str(" bytes @@\n");
        for line in content.split_inclusive('\n') {
            text.push('+');
            text.push_str(line);
            if !line.ends_with('\n') {
                text.push('\n');
            }
        }
        if content.is_empty() {
            text.push_str("+\n");
        }
    }

    Ok(UntrackedText {
        text,
        paths: included_paths,
    })
}

fn split_nul_paths(output: &str) -> Vec<String> {
    output
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

async fn git_stdout(worktree: &Path, args: Vec<String>) -> Result<String, GitError> {
    run_git_command(PathBuf::from(worktree), args)
        .await
        .map(|output| output.stdout)
}

fn sha256_hex(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{checkout_branch, git, init_repo_with_main_commit, write_and_commit};

    fn write(repo_path: &Path, relative_path: &str, contents: &str) {
        let path = repo_path.join(relative_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent directory");
        }
        std::fs::write(path, contents).expect("write fixture file");
    }

    async fn fingerprint(repo_path: &Path) -> SubmissionDiffFingerprint {
        compute_submission_diff_fingerprint(repo_path)
            .await
            .expect("compute fingerprint")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_diff_is_explicit_without_fingerprint() {
        let fixture = init_repo_with_main_commit();

        let result = fingerprint(fixture.path()).await;

        assert!(result.is_no_diff());
        assert_eq!(result.fingerprint(), None);
        assert_eq!(result.canonical_diff_len(), 0);
        assert!(result.changed_paths().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fingerprints_dirty_only_tracked_edits() {
        let fixture = init_repo_with_main_commit();
        write(fixture.path(), "README.md", "hello\ndirty\n");

        let result = fingerprint(fixture.path()).await;

        let digest = match result {
            SubmissionDiffFingerprint::Diff(digest) => digest,
            SubmissionDiffFingerprint::NoDiff(_) => panic!("dirty edit should produce a diff"),
        };
        assert_eq!(digest.changed_paths, vec!["README.md"]);
        assert_eq!(digest.fingerprint.len(), 64);
        assert!(digest.canonical_diff_len > 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fingerprints_local_committed_task_branch_changes() {
        let fixture = init_repo_with_main_commit();
        checkout_branch(fixture.path(), "task/local", Some("main"));
        write_and_commit(
            fixture.path(),
            "src/lib.rs",
            "pub fn task_change() {}\n",
            "task change",
        );

        let result = fingerprint(fixture.path()).await;

        let digest = match result {
            SubmissionDiffFingerprint::Diff(digest) => digest,
            SubmissionDiffFingerprint::NoDiff(_) => panic!("local commit should produce a diff"),
        };
        assert_eq!(digest.changed_paths, vec!["src/lib.rs"]);
        assert_eq!(digest.fingerprint.len(), 64);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fingerprints_staged_index_changes() {
        let fixture = init_repo_with_main_commit();
        write(fixture.path(), "staged.txt", "staged\n");
        git(fixture.path(), ["add", "staged.txt"]);

        let result = fingerprint(fixture.path()).await;

        let digest = match result {
            SubmissionDiffFingerprint::Diff(digest) => digest,
            SubmissionDiffFingerprint::NoDiff(_) => panic!("staged edit should produce a diff"),
        };
        assert_eq!(digest.changed_paths, vec!["staged.txt"]);
        assert_eq!(digest.fingerprint.len(), 64);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fingerprints_untracked_text_files_deterministically() {
        let fixture = init_repo_with_main_commit();
        write(fixture.path(), "zeta.txt", "z\n");
        write(fixture.path(), "alpha.txt", "a\n");
        write(fixture.path(), "binary.dat", "ok\0not text");

        let first = fingerprint(fixture.path()).await;
        let second = fingerprint(fixture.path()).await;

        let first_digest = match first {
            SubmissionDiffFingerprint::Diff(digest) => digest,
            SubmissionDiffFingerprint::NoDiff(_) => panic!("untracked text should produce a diff"),
        };
        assert_eq!(first_digest.changed_paths, vec!["alpha.txt", "zeta.txt"]);
        assert_eq!(
            Some(first_digest.fingerprint.as_str()),
            second.fingerprint()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn committed_task_branch_diff_is_not_fooled_by_matching_task_remote_head() {
        let fixture = init_repo_with_main_commit();
        checkout_branch(fixture.path(), "task/remote-matches-head", Some("main"));
        write_and_commit(
            fixture.path(),
            "remote-matches-head.txt",
            "committed but no worktree diff\n",
            "task commit",
        );
        git(
            fixture.path(),
            ["branch", "origin/task/remote-matches-head", "HEAD"],
        );
        let head_vs_task_remote = git(
            fixture.path(),
            ["diff", "origin/task/remote-matches-head", "HEAD"],
        );
        assert!(head_vs_task_remote.stdout.is_empty());

        let result = fingerprint(fixture.path()).await;

        let digest = match result {
            SubmissionDiffFingerprint::Diff(digest) => digest,
            SubmissionDiffFingerprint::NoDiff(_) => {
                panic!("merge-base(main)..HEAD task commit should produce a diff")
            }
        };
        assert_eq!(digest.changed_paths, vec!["remote-matches-head.txt"]);
    }
}
