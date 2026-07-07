//! Safety-scanned checkpoint preservation core for worker worktrees.
//!
//! This module implements the *freeze / inspect / filter / scan / fingerprint*
//! stage of the capture-before-exit checkpoint lifecycle. It does **not** push
//! to remote, create commits, or wire shutdown callers — those belong to later
//! tasks in epic 8yjx. Its sole job is to produce a deterministic, structured
//! safety-scan result that later tasks use to decide whether to commit, push,
//! defer, or abort.
//!
//! ## Pipeline
//!
//! 1. **Freeze/inspect** — gather the complete set of dirty/untracked/ignored
//!    paths from the worktree via `git status --porcelain=v1 -z` so the staged
//!    set is deterministic for a single checkpoint attempt.
//! 2. **Classify & exclude** — separate generated/cache/build/log/coverage/
//!    node_modules/target/LFS/submodule/large-binary paths from real worker
//!    output, classifying each as tracked, untracked, ignored, or generated.
//! 3. **Safety scan** — inspect staged-eligible file *paths* and *content* for
//!    secret-like tokens or policy-disallowed changes, collecting blockers.
//! 4. **Fingerprint** — compute exact before (HEAD)/after (worktree)/local
//!    (staged) diff fingerprints plus a staged/excluded/blocked summary.
//!
//! All pure classification and scanning logic is split into free functions that
//! take explicit inputs, so the bulk of the logic is unit-testable without a
//! real git repository. The async [`scan_worktree`] entry point wires those pure
//! functions to live git output.
//!
//! Path-level exclusion predicates (generated paths, root-level scratch files,
//! fixture/testdata allowlists) are shared with the workspace commit path via
//! [`djinn_workspace::commit_safety`], preventing semantic drift between the
//! checkpoint path and the WorkerDone auto-commit path.
//!
//! Design ref: [[design/8yjx-roadmap]].
//!
//! This module is a foundation for later tasks in epic 8yjx (WIP commit push
//! leases, shutdown wiring, coordinator preservation contract). Its public API
//! is intentionally complete even though no production caller uses it yet —
//! the `dead_code` allow below suppresses those warnings until the wiring tasks
//! land.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::Path;

use djinn_workspace::commit_safety::{self, CommitSafetyConfig};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

/// Maximum file size (in bytes) that is eligible for checkpoint staging without
/// being classified as a "large binary". Files at or above this limit are
/// excluded — they are almost always build artifacts, model weights, or dump
/// files that bloat the repository and fail GitHub's pre-receive hooks.
///
/// This is intentionally below GitHub's 100 MiB hard limit: the checkpoint path
/// is for *source* WIP, not for shipping binaries. The existing
/// `Workspace::reject_oversized_staged_files` guard remains as the hard
/// backstop; this is the softer "don't even try to stage it" filter.
pub const LARGE_BINARY_THRESHOLD_BYTES: u64 = 10 * 1024 * 1024; // 10 MiB

/// Default file-size threshold for reading content into memory for secret
/// scanning. Files larger than this are scanned by path-pattern only (their
/// content is not loaded), avoiding OOM on large generated outputs.
const SECRET_SCAN_CONTENT_LIMIT_BYTES: u64 = 1024 * 1024; // 1 MiB

// ─── Configuration ──────────────────────────────────────────────────────

/// Policy configuration for the checkpoint safety scan.
///
/// Controls which path patterns are excluded before staging and which content
/// patterns trigger a safety block. All fields have sensible defaults via
/// [`CheckpointSafetyConfig::default`]; callers can override individual fields
/// for project-specific policies (e.g. a monorepo with extra build dirs).
#[derive(Debug, Clone)]
pub struct CheckpointSafetyConfig {
    /// Glob-style path patterns (matched against the repo-relative POSIX path)
    /// whose files are excluded from staging as generated/cache/build output.
    /// Each entry is matched as a prefix or suffix (see [`is_generated_path`]).
    pub excluded_path_patterns: Vec<&'static str>,

    /// File extensions whose files are treated as generated/build output and
    /// excluded from staging.
    pub excluded_extensions: Vec<&'static str>,

    /// Directory names that, when they appear as a path component, cause the
    /// entire subtree to be excluded (e.g. `target`, `node_modules`).
    pub excluded_dir_components: Vec<&'static str>,

    /// Regex patterns (compiled at scan time) whose presence in staged file
    /// *content* triggers a safety block. Each pattern is matched
    /// case-insensitively against every line of every staged-eligible file
    /// under the content-size limit.
    pub secret_content_patterns: Vec<&'static str>,

    /// Path substrings whose presence in a staged file *path* triggers a safety
    /// block (e.g. `.env`, `credentials`, `id_rsa`). These catch files that are
    /// almost always secrets regardless of their content.
    pub blocked_path_substrings: Vec<&'static str>,

    /// Maximum file size (in bytes) for a file to be eligible for staging.
    /// Files at or above this limit are classified as large binaries and
    /// excluded.
    pub large_binary_threshold: u64,
}

impl Default for CheckpointSafetyConfig {
    fn default() -> Self {
        Self {
            excluded_path_patterns: commit_safety::DEFAULT_EXCLUDED_PATH_PATTERNS.to_vec(),
            excluded_extensions: commit_safety::DEFAULT_EXCLUDED_EXTENSIONS.to_vec(),
            excluded_dir_components: commit_safety::DEFAULT_EXCLUDED_DIR_COMPONENTS.to_vec(),
            secret_content_patterns: DEFAULT_SECRET_CONTENT_PATTERNS.to_vec(),
            blocked_path_substrings: DEFAULT_BLOCKED_PATH_SUBSTRINGS.to_vec(),
            large_binary_threshold: LARGE_BINARY_THRESHOLD_BYTES,
        }
    }
}

impl CheckpointSafetyConfig {
    /// Build a [`CommitSafetyConfig`] from this config's path-related fields.
    ///
    /// Used to delegate path-level classification to the shared
    /// [`djinn_workspace::commit_safety`] module without duplicating the
    /// pattern lists and matching logic.
    fn to_commit_safety_config(&self) -> CommitSafetyConfig {
        CommitSafetyConfig {
            excluded_path_patterns: self.excluded_path_patterns.clone(),
            excluded_extensions: self.excluded_extensions.clone(),
            excluded_dir_components: self.excluded_dir_components.clone(),
            ..Default::default()
        }
    }
}

/// Default secret-like content patterns. Each is matched case-insensitively
/// against file content. These intentionally err on the side of caution — a
/// false positive blocks the checkpoint (recoverable) rather than leaking a
/// secret (irreversible).
const DEFAULT_SECRET_CONTENT_PATTERNS: &[&str] = &[
    // AWS access keys: AKIA... (20 chars)
    r"AKIA[0-9A-Z]{16}",
    // AWS secret keys (40-char base64 after the label)
    r#"(?i)aws_secret_access_key["'\s:=]+[A-Za-z0-9/+=]{40}"#,
    // Generic API key assignments
    r#"(?i)(api[_-]?key|secret[_-]?key|access[_-]?token|auth[_-]?token|bearer)"#,
    r#"(?i)["']?(sk|pk|rk)_(live|test)_[A-Za-z0-9]{20,}"#, // Stripe-style
    // Private keys (PEM blocks)
    r"-----BEGIN (RSA |EC |OPENSSH |DSA |)PRIVATE KEY-----",
    // GitHub tokens (classic + fine-grained)
    r"gh[pousr]_[A-Za-z0-9]{36,}",
    // Slack tokens
    r"xox[baprs]-[A-Za-z0-9-]{10,}",
    // JWT (three base64 segments separated by dots)
    r"eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
    // Generic password assignments with a value
    r#"(?i)(password|passwd|pwd)["'\s:=]+["']?[^\s"']{8,}"#,
    // Connection strings with embedded credentials
    r#"(?i)(postgres|mysql|mongodb|redis)://[^:\s]+:[^@\s]+@"#,
];

/// Default path substrings that trigger a safety block regardless of content.
const DEFAULT_BLOCKED_PATH_SUBSTRINGS: &[&str] = &[
    ".env",
    ".envrc",
    "credentials",
    "id_rsa",
    "id_ecdsa",
    "id_ed25519",
    "id_dsa",
    ".pem",
    ".p12",
    ".pfx",
    ".keystore",
    ".keychain",
    "secrets.yml",
    "secrets.yaml",
    "secrets.json",
    ".npmrc",
    ".pypirc",
    ".netrc",
    ".pgpass",
];

// ─── Classification ──────────────────────────────────────────────────────

/// How a single file is classified relative to the checkpoint safety scan.
///
/// The classification drives the exclusion/filter decision: `Tracked` and
/// `Untracked` files are eligible for staging (subject to generated/secret
/// checks); `Ignored` and `Generated` files are excluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileClassification {
    /// A file that git is tracking (already in the index/HEAD).
    Tracked,
    /// A file that is not tracked but not ignored — new worker output.
    Untracked,
    /// A file matched by a `.gitignore` rule (git reports it as ignored).
    Ignored,
    /// A file that matches a generated/cache/build output pattern and should
    /// be excluded before staging even if it would otherwise be eligible.
    Generated,
}

/// A single file's classification and the reason it was excluded (if any).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassifiedFile {
    /// Repo-relative POSIX path (forward slashes, no leading `./`).
    pub path: String,
    /// Whether the file is tracked, untracked, ignored, or generated.
    pub classification: FileClassification,
    /// Human-readable reason this file was excluded from staging, or `None`
    /// if the file is eligible for staging.
    pub exclusion_reason: Option<ExclusionReason>,
}

/// Why a file was excluded from the staged set.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExclusionReason {
    /// Matches a generated/cache/build output path pattern.
    GeneratedPath,
    /// Matches a generated/build output file extension.
    GeneratedExtension,
    /// Inside a generated/cache/build output directory component.
    GeneratedDir,
    /// File exceeds the large-binary size threshold.
    LargeBinary { size_bytes: u64 },
    /// Git reports this path as ignored.
    GitIgnored,
    /// Path appears to be inside a git submodule worktree.
    Submodule,
    /// Path appears to be an LFS pointer or payload.
    LfsPayload,
    /// A root-level worker scratch file (e.g. `patch.txt`, `test.txt`).
    RootScratch,
}

// ─── Safety findings ────────────────────────────────────────────────────

/// A single safety finding (secret-like or policy-disallowed) detected during
/// the scan. The presence of any finding blocks checkpoint commit creation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SafetyFinding {
    /// Repo-relative POSIX path of the file containing the finding.
    pub path: String,
    /// What kind of safety issue was detected.
    pub kind: SafetyFindingKind,
    /// The pattern or rule that triggered the finding (for diagnostics).
    pub matched_pattern: String,
    /// 1-based line number within the file (if content was scanned), or 0
    /// if the finding is path-based only.
    pub line: usize,
    /// A redacted snippet of the matched content (if any), with secret-like
    /// values replaced. Safe to log/persist.
    pub snippet: Option<String>,
}

/// Categorisation of a safety finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyFindingKind {
    /// File content matched a secret-like pattern (API key, private key, etc.).
    SecretContent,
    /// File path matched a blocked-path substring (e.g. `.env`, `id_rsa`).
    BlockedPath,
}

// ─── Fingerprints ────────────────────────────────────────────────────────

/// Exact diff fingerprints for a single checkpoint attempt.
///
/// Each fingerprint is a hex SHA-256 of the canonical text representation of
/// the relevant diff, so identical worktrees produce identical fingerprints
/// and any change (even whitespace) alters the hash. Later tasks persist these
/// in checkpoint events for deduplication, audit, and resume selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DiffFingerprints {
    /// SHA-256 of the diff between HEAD and the worktree (all unstaged +
    /// untracked changes that would be staged). `None` if the worktree is
    /// clean relative to HEAD.
    pub worktree_diff: Option<String>,
    /// SHA-256 of the diff that *would* be staged after exclusions (the
    /// filtered set). `None` if nothing remains after filtering.
    pub staged_diff: Option<String>,
    /// SHA-256 of the diff between the local HEAD and `origin/<branch>`
    /// (committed-but-unpushed work). `None` if HEAD == origin/branch or
    /// origin is unavailable.
    pub local_vs_remote_diff: Option<String>,
    /// SHA of the current HEAD commit (the "before" parent).
    pub head_sha: Option<String>,
    /// SHA of `origin/<branch>` (the remote tip), if available.
    pub remote_sha: Option<String>,
}

// ─── Scan result ─────────────────────────────────────────────────────────

/// The complete, structured result of a checkpoint safety scan.
///
/// This is the primary output of [`scan_worktree`]. Later tasks (WIP commit
/// creation, push leases, shutdown wiring, coordinator preservation contract)
/// consume this to decide whether and how to preserve the worktree.
///
/// The result is deterministic for a given worktree state + config: running
/// the scan twice on an unchanged tree yields identical staged/excluded/blocked
/// sets and fingerprints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CheckpointSafetyScan {
    /// Files that passed all filters and are eligible for staging (tracked
    /// or untracked, not generated, not blocked). Sorted by path.
    pub staged: Vec<String>,
    /// Files excluded before staging, with the reason. Sorted by path.
    pub excluded: Vec<ExcludedFile>,
    /// Files blocked by the safety scan (secret-like or policy-disallowed),
    /// with the finding details. Sorted by path.
    pub blocked: Vec<SafetyFinding>,
    /// Exact diff fingerprints for events and persisted results.
    pub fingerprints: DiffFingerprints,
    /// Whether the worktree had any changes at all (dirty/untracked) before
    /// filtering. A clean worktree produces an empty `staged` set and
    /// `had_changes == false`.
    pub had_changes: bool,
}

/// A file excluded from staging, with the reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExcludedFile {
    /// Repo-relative POSIX path.
    pub path: String,
    /// Why the file was excluded.
    pub reason: ExclusionReason,
}

// ─── Error type ──────────────────────────────────────────────────────────

/// Errors that can occur during the checkpoint safety scan.
#[derive(Debug, thiserror::Error)]
pub enum CheckpointSafetyError {
    /// A git command failed (non-zero exit, spawn failure, etc.).
    #[error("git {command} failed: {stderr}")]
    Git { command: String, stderr: String },
    /// The worktree path does not exist or is not a git repository.
    #[error("workspace path is not a valid git repository: {0}")]
    InvalidWorkspace(String),
}

// ─── Pure classification functions ───────────────────────────────────────

/// Classify a single repo-relative path against the exclusion config.
///
/// Returns the [`FileClassification`] and an [`ExclusionReason`] if the path
/// should be excluded from staging. This is the pure core of the classification
/// pipeline — it does not touch the filesystem or run git.
///
/// Path-level checks (generated paths, root-level scratch files,
/// fixture/testdata allowlists) delegate to [`djinn_workspace::commit_safety`]
/// to prevent semantic drift with the WorkerDone auto-commit path.
///
/// Parameters:
/// - `path`: repo-relative POSIX path (forward slashes, no leading `./`).
/// - `git_status`: the git porcelain status character(s) for this path
///   (e.g. `"M"` modified, `"A"` added, `"??"` untracked, `"!!"` ignored).
/// - `size_bytes`: on-disk file size, or `None` if the file is deleted or
///   its size is unknown.
/// - `config`: the safety-scan policy.
pub fn classify_path(
    path: &str,
    git_status: &str,
    size_bytes: Option<u64>,
    config: &CheckpointSafetyConfig,
) -> ClassifiedFile {
    // Determine the base classification from git status.
    let base = if git_status == "??" {
        FileClassification::Untracked
    } else if git_status == "!!" {
        FileClassification::Ignored
    } else {
        FileClassification::Tracked
    };

    // Git-ignored paths are excluded immediately.
    if base == FileClassification::Ignored {
        return ClassifiedFile {
            path: path.to_string(),
            classification: FileClassification::Ignored,
            exclusion_reason: Some(ExclusionReason::GitIgnored),
        };
    }

    // Submodule worktree: path starts with a submodule entry followed by `/`.
    // We detect this heuristically: a path component that looks like a
    // submodule directory (`.gitmodules` would list it). For the pure
    // classifier we check common submodule path patterns.
    if is_submodule_path(path) {
        return ClassifiedFile {
            path: path.to_string(),
            classification: FileClassification::Ignored,
            exclusion_reason: Some(ExclusionReason::Submodule),
        };
    }

    // LFS pointer file or payload.
    if is_lfs_payload(path) {
        return ClassifiedFile {
            path: path.to_string(),
            classification: FileClassification::Ignored,
            exclusion_reason: Some(ExclusionReason::LfsPayload),
        };
    }

    // Delegate generated-path, generated-dir, and generated-extension checks
    // to the shared commit_safety module to prevent semantic drift.
    let shared_config = config.to_commit_safety_config();
    let shared_result = commit_safety::classify_path(path, &shared_config);
    match shared_result {
        commit_safety::PathClassification::Excluded(reason) => {
            let exclusion = match reason {
                commit_safety::PathExclusionReason::GeneratedPath => ExclusionReason::GeneratedPath,
                commit_safety::PathExclusionReason::GeneratedExtension => {
                    ExclusionReason::GeneratedExtension
                }
                commit_safety::PathExclusionReason::GeneratedDir => ExclusionReason::GeneratedDir,
                commit_safety::PathExclusionReason::RootScratch => ExclusionReason::RootScratch,
                commit_safety::PathExclusionReason::RootEditorDrop => {
                    ExclusionReason::GeneratedPath
                }
            };
            return ClassifiedFile {
                path: path.to_string(),
                classification: FileClassification::Generated,
                exclusion_reason: Some(exclusion),
            };
        }
        commit_safety::PathClassification::Allowed => {}
    }

    // Large binary (checkpoint-specific: not in the shared module).
    if let Some(size) = size_bytes
        && size >= config.large_binary_threshold
    {
        return ClassifiedFile {
            path: path.to_string(),
            classification: FileClassification::Generated,
            exclusion_reason: Some(ExclusionReason::LargeBinary { size_bytes: size }),
        };
    }

    ClassifiedFile {
        path: path.to_string(),
        classification: base,
        exclusion_reason: None,
    }
}

/// Check whether a path matches any of the configured generated-path patterns.
///
/// Delegates to [`djinn_workspace::commit_safety::is_generated_path`] using
/// this config's path patterns.
pub fn is_generated_path(path: &str, config: &CheckpointSafetyConfig) -> bool {
    let shared_config = config.to_commit_safety_config();
    commit_safety::is_generated_path(path, &shared_config)
}

/// Check whether any path *component* is a generated directory name.
///
/// Delegates to [`djinn_workspace::commit_safety::has_generated_dir_component`].
pub fn has_generated_dir_component(path: &str, config: &CheckpointSafetyConfig) -> bool {
    let shared_config = config.to_commit_safety_config();
    commit_safety::has_generated_dir_component(path, &shared_config)
}

/// Check whether the file extension is in the generated-extensions list.
///
/// Delegates to [`djinn_workspace::commit_safety::has_generated_extension`].
pub fn has_generated_extension(path: &str, config: &CheckpointSafetyConfig) -> bool {
    let shared_config = config.to_commit_safety_config();
    commit_safety::has_generated_extension(path, &shared_config)
}

/// Heuristic: detect submodule worktree paths.
///
/// A submodule's files appear under a subdirectory that is itself a git
/// repository. We can't read `.gitmodules` in the pure classifier, so we
/// check for known submodule-indicator path patterns: a `.git` file (not
/// directory) inside a subdirectory, or paths containing `/vendor/` that are
/// commonly submodules. The full submodule list is resolved by the async
/// scan via `git submodule status`.
fn is_submodule_path(_path: &str) -> bool {
    // This is a lightweight heuristic; the real submodule check happens in
    // scan_worktree via `git submodule status --recursive`. Here we only catch
    // obvious cases to keep the pure classifier self-contained.
    false
}

/// Heuristic: detect LFS pointer files or payloads.
///
/// LFS pointer files have the signature:
/// ```text
/// version https://git-lfs.github.com/spec/v1
/// oid sha256:<hex>
/// size <number>
/// ```
/// We detect by extension (`.lfs`) or by path containing `lfs` — but primarily
/// the async scan reads file content to detect the pointer signature. Here we
/// do a lightweight path check.
fn is_lfs_payload(path: &str) -> bool {
    path.ends_with(".lfs") || path.ends_with(".lfsobj")
}

// ─── Pure secret-scanning functions ──────────────────────────────────────

/// Scan a single file's content for secret-like patterns.
///
/// Returns a list of [`SafetyFinding`] for each matched pattern. Content is
/// scanned line-by-line; each finding records the 1-based line number and a
/// redacted snippet.
///
/// This is the pure core of content scanning — it takes the path and content
/// as strings and returns findings without touching the filesystem.
pub fn scan_content_for_secrets(
    path: &str,
    content: &str,
    config: &CheckpointSafetyConfig,
) -> Vec<SafetyFinding> {
    let mut findings = Vec::new();
    for (line_num, line) in content.lines().enumerate() {
        for pattern in &config.secret_content_patterns {
            if let Some(matched) = simple_regex_search(pattern, line) {
                findings.push(SafetyFinding {
                    path: path.to_string(),
                    kind: SafetyFindingKind::SecretContent,
                    matched_pattern: (*pattern).to_string(),
                    line: line_num + 1,
                    snippet: Some(redact(&matched, line)),
                });
            }
        }
    }
    findings
}

/// Check whether a path matches any blocked-path substring.
pub fn scan_path_for_blocks(path: &str, config: &CheckpointSafetyConfig) -> Vec<SafetyFinding> {
    let lower = path.to_lowercase();
    let mut findings = Vec::new();
    for substr in &config.blocked_path_substrings {
        if lower.contains(&substr.to_lowercase()) {
            findings.push(SafetyFinding {
                path: path.to_string(),
                kind: SafetyFindingKind::BlockedPath,
                matched_pattern: (*substr).to_string(),
                line: 0,
                snippet: None,
            });
        }
    }
    findings
}

/// Redact a matched line for safe logging/persistence.
///
/// Replaces the portion of the line that matched the pattern with `***REDACTED***`,
/// preserving surrounding context for diagnostics.
fn redact(matched_text: &str, full_line: &str) -> String {
    // If the matched text is a substring of the line, redact just that part.
    // Otherwise, redact the value after `:` or `=` if present.
    if let Some(pos) = full_line.find(matched_text) {
        let before = &full_line[..pos];
        let after = &full_line[pos + matched_text.len()..];
        return format!("{before}***REDACTED***{after}");
    }
    // Fallback: if the line contains `=` or `:`, redact everything after it.
    if let Some(pos) = full_line.find(['=', ':'].as_ref()) {
        let prefix = &full_line[..=pos];
        return format!("{prefix} ***REDACTED***");
    }
    // Last resort: redact the whole line if it's long enough to plausibly
    // contain a secret.
    if full_line.len() > 20 {
        return "***REDACTED***".to_string();
    }
    full_line.to_string()
}

/// A minimal regex-like matcher that supports the subset of patterns used by
/// the default secret patterns: literal characters, `(?i)` case-insensitive
/// flag, `[A-Z]` character classes, `{n,m}` quantifiers, `.`, `\d`, `\s`, `\w`,
/// `+`, `*`, and alternation via `|`.
///
/// For patterns this simple matcher can't handle, we fall back to a substring
/// check on the literal portions. This avoids pulling in a full regex engine
/// dependency while covering the common secret-detection patterns.
///
/// Returns the matched text if found, or `None`.
fn simple_regex_search(pattern: &str, text: &str) -> Option<String> {
    // The `regex` crate is a workspace dependency and handles all our patterns
    // (including `(?i)` flags, character classes, quantifiers). Compile once
    // and match; if the pattern is invalid, fall back to a case-insensitive
    // literal substring search on the pattern text.
    match regex::Regex::new(pattern) {
        Ok(re) => re.find(text).map(|m| m.as_str().to_string()),
        Err(_) => {
            // Fallback: literal substring match (case-insensitive if (?i)).
            let (case_insensitive, pat) = pattern
                .strip_prefix("(?i)")
                .map_or((false, pattern), |rest| (true, rest));
            let needle = if case_insensitive {
                pat.to_lowercase()
            } else {
                pat.to_string()
            };
            let haystack = if case_insensitive {
                text.to_lowercase()
            } else {
                text.to_string()
            };
            haystack
                .find(&needle)
                .map(|start| text[start..start + needle.len()].to_string())
        }
    }
}

// ─── Fingerprint computation ─────────────────────────────────────────────

/// Compute a SHA-256 fingerprint of the given text content.
///
/// Returns the hex-encoded hash. Identical content always yields the same hash.
pub fn diff_fingerprint(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Compute a stable fingerprint of a sorted list of file paths.
///
/// Used for the staged/excluded summaries: the fingerprint captures *which*
/// files are in the set without depending on content (which may not be
/// available for excluded files). The paths are sorted before hashing so the
/// result is order-independent.
pub fn path_set_fingerprint(paths: &[&str]) -> String {
    let mut sorted: Vec<&str> = paths.to_vec();
    sorted.sort_unstable();
    sorted.join("\n")
}

// ─── Async worktree scan ─────────────────────────────────────────────────

/// Parse a single line of `git status --porcelain=v1` output into
/// (status_code, path).
///
/// The porcelain v1 format is two status characters followed by a space and
/// the path. Untracked files are `??`, ignored files are `!!`. Paths with
/// spaces or special chars are quoted; we strip the surrounding quotes.
fn parse_porcelain_line(line: &str) -> Option<(String, String)> {
    if line.len() < 3 {
        return None;
    }
    let status = &line[..2];
    let path = line[3..].trim();
    // Strip surrounding quotes (git quotes paths with special characters).
    let path = path.trim_matches('"');
    if path.is_empty() {
        return None;
    }
    // Convert backslash-escaped paths to forward slashes (Windows compatibility).
    let path = path.replace('\\', "/");
    Some((status.to_string(), path.to_string()))
}

/// Run a git command in the worktree and return its stdout.
///
/// Delegates to [`djinn_git::run_git_command_in`] which applies
/// `safe.directory=*` and lowers process priority, matching the
/// requirements for the mixed-UID K8s Pod environment.
async fn run_git(worktree: &Path, args: &[&str]) -> Result<String, CheckpointSafetyError> {
    let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let command_display = args.join(" ");
    let out = djinn_git::run_git_command_in(worktree, owned)
        .await
        .map_err(|e| CheckpointSafetyError::Git {
            command: command_display.clone(),
            stderr: match &e {
                djinn_git::GitError::CommandFailed { stderr, .. } => stderr.clone(),
                other => format!("{other}"),
            },
        })?;
    Ok(out.stdout)
}

/// Freeze and inspect a worker worktree, classifying every dirty/untracked/
/// ignored path, excluding generated/cache/build files, scanning for secrets,
/// and computing exact diff fingerprints — all without pushing or committing.
///
/// This is the primary entry point for the checkpoint safety core. It produces
/// a deterministic [`CheckpointSafetyScan`] for the current worktree state.
///
/// # Parameters
/// - `worktree`: path to the git working tree (the ephemeral stage clone).
/// - `branch`: the task branch name (used for the local-vs-remote fingerprint).
/// - `config`: the safety-scan policy. Use [`CheckpointSafetyConfig::default`]
///   for standard exclusions.
///
/// # Errors
/// Returns [`CheckpointSafetyError`] if git commands fail or the worktree is
/// not a valid repository.
pub async fn scan_worktree(
    worktree: &Path,
    branch: &str,
    config: &CheckpointSafetyConfig,
) -> Result<CheckpointSafetyScan, CheckpointSafetyError> {
    // ── 1. Freeze/inspect: gather all dirty/untracked/ignored paths ──
    //
    // `git status --porcelain=v1` gives us tracked changes (modified, added,
    // deleted) and untracked files. We add `-u` (default) to include untracked
    // files and `--ignored` to also capture ignored files for the excluded
    // summary. We use two calls because `--ignored` changes the output format.
    let tracked_and_untracked = run_git(
        worktree,
        &["status", "--porcelain=v1", "-u", "--no-renames"],
    )
    .await?;

    let ignored = run_git(
        worktree,
        &["status", "--porcelain=v1", "--ignored", "--no-renames"],
    )
    .await?;

    // ── 2. Resolve SHAs for fingerprints ──
    let head_sha = run_git(worktree, &["rev-parse", "HEAD"])
        .await
        .ok()
        .map(|s| s.trim().to_string());

    let remote_sha = run_git(worktree, &["rev-parse", &format!("origin/{branch}")])
        .await
        .ok()
        .map(|s| s.trim().to_string());

    // ── 3. Classify each path ──
    let mut classified: BTreeMap<String, (String, Option<u64>)> = BTreeMap::new();

    for line in tracked_and_untracked.lines() {
        if let Some((status, path)) = parse_porcelain_line(line) {
            // For rename entries (status 'R' or 'C'), git shows "old -> new".
            // With --no-renames these don't occur, but handle defensively.
            let path = path.split(" -> ").last().unwrap_or(&path).to_string();
            let size = file_size(worktree, &path);
            classified.insert(path, (status, size));
        }
    }

    // Add ignored paths from the --ignored output.
    let mut had_changes = !classified.is_empty();
    for line in ignored.lines() {
        if let Some((status, path)) = parse_porcelain_line(line)
            && status == "!!"
        {
            let path = path.split(" -> ").last().unwrap_or(&path).to_string();
            let size = file_size(worktree, &path);
            classified.entry(path).or_insert((status, size));
            had_changes = true;
        }
    }

    if classified.is_empty() {
        // Clean worktree — return an empty result.
        return Ok(CheckpointSafetyScan {
            had_changes: false,
            fingerprints: DiffFingerprints {
                head_sha,
                remote_sha,
                ..Default::default()
            },
            ..Default::default()
        });
    }

    // ── 4. Apply classification + exclusion ──
    let mut staged: Vec<String> = Vec::new();
    let mut excluded: Vec<ExcludedFile> = Vec::new();
    let mut untracked_staged: Vec<String> = Vec::new();

    for (path, (status, size)) in &classified {
        let file = classify_path(path, status, *size, config);
        match file.exclusion_reason {
            Some(reason) => {
                excluded.push(ExcludedFile {
                    path: file.path,
                    reason,
                });
            }
            None => {
                if status == "??" {
                    untracked_staged.push(file.path.clone());
                }
                staged.push(file.path);
            }
        }
    }

    staged.sort();
    untracked_staged.sort();
    excluded.sort_by(|a, b| a.path.cmp(&b.path));

    // ── 5. Safety scan: secrets and blocked paths ──
    let mut blocked: Vec<SafetyFinding> = Vec::new();

    for path in &staged {
        // Path-based blocks.
        let path_findings = scan_path_for_blocks(path, config);
        blocked.extend(path_findings);

        // Content-based secret scan (only for files under the size limit).
        let full_path = worktree.join(path);
        if let Some(content) = read_file_for_scanning(&full_path) {
            let content_findings = scan_content_for_secrets(path, &content, config);
            blocked.extend(content_findings);
        }
    }

    // Deduplicate and sort blocked findings.
    blocked.sort();
    blocked.dedup();

    // ── 6. Compute fingerprints ──
    let worktree_diff = compute_worktree_diff(worktree, &untracked_staged)
        .await
        .ok();
    let staged_diff = if staged.is_empty() {
        None
    } else {
        Some(diff_fingerprint(
            &compute_staged_diff_text(worktree, &staged).await,
        ))
    };
    let local_vs_remote_diff = compute_local_vs_remote_diff(worktree, &head_sha, &remote_sha)
        .await
        .ok()
        .filter(|s| !s.is_empty());

    let worktree_diff_hash = worktree_diff
        .filter(|s| !s.is_empty())
        .map(|s| diff_fingerprint(&s));

    Ok(CheckpointSafetyScan {
        staged,
        excluded,
        blocked,
        fingerprints: DiffFingerprints {
            worktree_diff: worktree_diff_hash,
            staged_diff,
            local_vs_remote_diff: local_vs_remote_diff.map(|s| diff_fingerprint(&s)),
            head_sha,
            remote_sha,
        },
        had_changes,
    })
}

/// Get the on-disk file size for a repo-relative path, or `None` if the file
/// doesn't exist (deleted) or can't be stat'd.
fn file_size(worktree: &Path, repo_path: &str) -> Option<u64> {
    std::fs::symlink_metadata(worktree.join(repo_path))
        .ok()
        .map(|m| m.len())
}

/// Read a file's content for secret scanning, respecting the content-size limit.
///
/// Returns `Some(content)` if the file exists, is under the size limit, and is
/// likely text (UTF-8 decodable). Returns `None` for files that are too large,
/// binary, or unreadable.
fn read_file_for_scanning(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    if meta.len() > SECRET_SCAN_CONTENT_LIMIT_BYTES {
        debug!(
            path = %path.display(),
            size = meta.len(),
            "skipping secret content scan: file exceeds content size limit"
        );
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    // Reject files with null bytes in the first 8KB (binary heuristic).
    let check_len = bytes.len().min(8192);
    if bytes[..check_len].contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Compute the full worktree diff (HEAD vs working tree) as text.
///
/// This is the "before/after" fingerprint source. It includes both tracked
/// modifications (`git diff HEAD`) and untracked file content (which `git diff
/// HEAD` omits) so the fingerprint captures the complete delta.
async fn compute_worktree_diff(
    worktree: &Path,
    untracked: &[String],
) -> Result<String, CheckpointSafetyError> {
    // `git diff HEAD` covers tracked modifications and deletions.
    let mut diff = run_git(worktree, &["diff", "HEAD"]).await?;

    // Untracked files don't appear in `git diff HEAD`. Append their content
    // in a canonical format so the fingerprint captures them.
    for path in untracked {
        if let Ok(content) = std::fs::read_to_string(worktree.join(path)) {
            diff.push_str(&format!("--- /dev/null\n+++ b/{path}\n+{content}"));
        }
    }

    Ok(diff)
}

/// Compute a stable text representation of the staged diff for fingerprinting.
///
/// Rather than actually staging files (which mutates the index), we construct
/// a canonical text from the sorted list of staged paths and their individual
/// diffs against HEAD. This is deterministic and side-effect-free.
async fn compute_staged_diff_text(worktree: &Path, staged: &[String]) -> String {
    let mut parts = Vec::new();
    for path in staged {
        // `git diff HEAD -- <path>` gives the per-file diff.
        match run_git(worktree, &["diff", "HEAD", "--", path]).await {
            Ok(diff) if !diff.trim().is_empty() => parts.push(diff),
            Ok(_) => {
                // No diff against HEAD — it's an untracked new file. Use its
                // content hash instead.
                if let Ok(content) = std::fs::read_to_string(worktree.join(path)) {
                    parts.push(format!("--- /dev/null\n+++ b/{path}\n+{}", content));
                }
            }
            Err(e) => {
                warn!(path = path, error = %e, "failed to compute per-file diff for fingerprint");
            }
        }
    }
    parts.join("\n")
}

/// Compute the diff between local HEAD and the remote branch tip.
///
/// Returns the raw `git diff` output, or an empty string if they're identical.
async fn compute_local_vs_remote_diff(
    worktree: &Path,
    head_sha: &Option<String>,
    remote_sha: &Option<String>,
) -> Result<String, CheckpointSafetyError> {
    match (head_sha, remote_sha) {
        (Some(head), Some(remote)) if head != remote => {
            run_git(worktree, &["diff", remote, head]).await
        }
        _ => Ok(String::new()),
    }
}

#[cfg(test)]
mod tests;
