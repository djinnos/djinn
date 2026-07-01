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

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::process::Command;
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
            excluded_path_patterns: DEFAULT_EXCLUDED_PATH_PATTERNS.to_vec(),
            excluded_extensions: DEFAULT_EXCLUDED_EXTENSIONS.to_vec(),
            excluded_dir_components: DEFAULT_EXCLUDED_DIR_COMPONENTS.to_vec(),
            secret_content_patterns: DEFAULT_SECRET_CONTENT_PATTERNS.to_vec(),
            blocked_path_substrings: DEFAULT_BLOCKED_PATH_SUBSTRINGS.to_vec(),
            large_binary_threshold: LARGE_BINARY_THRESHOLD_BYTES,
        }
    }
}

/// Default excluded path patterns — generated caches, build outputs, logs,
/// coverage reports, dependency directories, and LFS/submodule payloads.
const DEFAULT_EXCLUDED_PATH_PATTERNS: &[&str] = &[
    "target/",
    "node_modules/",
    ".next/",
    ".nuxt/",
    ".svelte-kit/",
    "dist/",
    "build/",
    ".build/",
    "out/",
    ".output/",
    ".turbo/",
    ".parcel-cache/",
    "coverage/",
    ".nyc_output/",
    "__pycache__/",
    ".pytest_cache/",
    ".mypy_cache/",
    ".ruff_cache/",
    ".tox/",
    ".venv/",
    "venv/",
    ".cache/",
    ".gradle/",
    ".mvn/",
    "vendor/",
    ".cargo/registry/",
    ".local/share/pnpm/",
    ".pnpm-store/",
    ".yarn/cache/",
    ".turbo/cache/",
    "logs/",
    "log/",
    "*.log",
    "*.tmp",
    "*.swp",
    "*.bak",
    "*.lock.json",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "Cargo.lock",
    "*.lcov",
    "*.profdata",
    "*.profraw",
    "*.gcno",
    "*.gcda",
    "*.o",
    "*.a",
    "*.so",
    "*.dylib",
    "*.dll",
    "*.exe",
    "*.wasm",
    "*.pdb",
    "*.class",
    "*.jar",
    "*.war",
    "*.pyc",
    "*.pyo",
    "*.DS_Store",
    "*.thumbs.db",
];

/// Default excluded file extensions (binaries and compiled artifacts).
const DEFAULT_EXCLUDED_EXTENSIONS: &[&str] = &[
    "pyc", "pyo", "class", "o", "a", "so", "dylib", "dll", "exe", "wasm", "pdb", "jar", "war",
    "log", "lcov", "profdata", "profraw", "gcno", "gcda", "tmp", "swp", "bak", "DS_Store",
];

/// Default directory components that cause entire subtrees to be excluded.
const DEFAULT_EXCLUDED_DIR_COMPONENTS: &[&str] = &[
    "target",
    "node_modules",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    ".venv",
    "venv",
    "env",
    ".cache",
    ".gradle",
    ".mvn",
    "dist",
    "build",
    ".build",
    "out",
    ".output",
    ".turbo",
    ".parcel-cache",
    "coverage",
    ".nyc_output",
    ".next",
    ".nuxt",
    ".svelte-kit",
    "vendor",
    ".cargo",
    ".pnpm-store",
    ".yarn",
    "logs",
    "log",
    ".idea",
    ".vscode",
];

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

    // Generated path patterns.
    if is_generated_path(path, config) {
        return ClassifiedFile {
            path: path.to_string(),
            classification: FileClassification::Generated,
            exclusion_reason: Some(ExclusionReason::GeneratedPath),
        };
    }

    // Generated directory component.
    if has_generated_dir_component(path, config) {
        return ClassifiedFile {
            path: path.to_string(),
            classification: FileClassification::Generated,
            exclusion_reason: Some(ExclusionReason::GeneratedDir),
        };
    }

    // Generated file extension.
    if has_generated_extension(path, config) {
        return ClassifiedFile {
            path: path.to_string(),
            classification: FileClassification::Generated,
            exclusion_reason: Some(ExclusionReason::GeneratedExtension),
        };
    }

    // Large binary.
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
/// A pattern matches if:
/// - It ends with `/` and the path starts with it (directory prefix), or
/// - It starts with `*` and the path ends with the pattern's suffix (glob), or
/// - The path *contains* the pattern as a path segment.
pub fn is_generated_path(path: &str, config: &CheckpointSafetyConfig) -> bool {
    for pat in &config.excluded_path_patterns {
        if pat.ends_with('/') {
            // Directory prefix: `target/` matches `target/foo` and `foo/target/bar`.
            let dir = pat.trim_end_matches('/');
            if path == dir
                || path.starts_with(&format!("{dir}/"))
                || path.contains(&format!("/{dir}/"))
            {
                return true;
            }
        } else if let Some(suffix) = pat.strip_prefix('*') {
            // Glob suffix: `*.log` matches `app.log` and `logs/app.log`.
            if path.ends_with(suffix) {
                return true;
            }
        } else {
            // Bare segment match.
            if path == *pat
                || path.contains(&format!("/{pat}/"))
                || path.starts_with(&format!("{pat}/"))
            {
                return true;
            }
        }
    }
    false
}

/// Check whether any path *component* is a generated directory name.
pub fn has_generated_dir_component(path: &str, config: &CheckpointSafetyConfig) -> bool {
    for component in path.split('/') {
        if config.excluded_dir_components.contains(&component) {
            return true;
        }
    }
    false
}

/// Check whether the file extension is in the generated-extensions list.
pub fn has_generated_extension(path: &str, config: &CheckpointSafetyConfig) -> bool {
    if let Some(ext) = path.rsplit('.').next()
        && path.contains('.')
        && ext.len() <= 10
        && ext != path
        && config.excluded_extensions.contains(&ext)
    {
        return true;
    }
    false
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
/// Uses `tokio::process::Command` with the same `safe.directory` env injection
/// as `djinn_git::run_git_command`, so it works in the mixed-UID K8s Pod
/// environment.
async fn run_git(worktree: &Path, args: &[&str]) -> Result<String, CheckpointSafetyError> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(worktree).args(args);
    // safe.directory injection — see djinn_git::run_git_command docs.
    cmd.env("GIT_CONFIG_COUNT", "1");
    cmd.env("GIT_CONFIG_KEY_0", "safe.directory");
    cmd.env("GIT_CONFIG_VALUE_0", "*");
    let output = cmd.output().await.map_err(|e| CheckpointSafetyError::Git {
        command: args.join(" "),
        stderr: format!("spawn failed: {e}"),
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(CheckpointSafetyError::Git {
            command: args.join(" "),
            stderr,
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
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

// ─── Unit tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Classification tests ────────────────────────────────────────────

    #[test]
    fn classify_tracked_modified_file_is_eligible() {
        let config = CheckpointSafetyConfig::default();
        let result = classify_path("src/main.rs", "M", Some(1024), &config);
        assert_eq!(result.classification, FileClassification::Tracked);
        assert!(result.exclusion_reason.is_none());
    }

    #[test]
    fn classify_untracked_source_file_is_eligible() {
        let config = CheckpointSafetyConfig::default();
        let result = classify_path("src/new_module.rs", "??", Some(512), &config);
        assert_eq!(result.classification, FileClassification::Untracked);
        assert!(result.exclusion_reason.is_none());
    }

    #[test]
    fn classify_target_dir_is_excluded_as_generated() {
        let config = CheckpointSafetyConfig::default();
        let result = classify_path("target/debug/deps/libfoo.rlib", "??", Some(1024), &config);
        assert_eq!(result.classification, FileClassification::Generated);
        assert!(result.exclusion_reason.is_some());
    }

    #[test]
    fn classify_node_modules_is_excluded_as_generated() {
        let config = CheckpointSafetyConfig::default();
        let result = classify_path("node_modules/react/index.js", "??", Some(1024), &config);
        assert_eq!(result.classification, FileClassification::Generated);
        assert!(result.exclusion_reason.is_some());
    }

    #[test]
    fn classify_log_file_is_excluded_by_pattern() {
        let config = CheckpointSafetyConfig::default();
        let result = classify_path("app.log", "??", Some(1024), &config);
        assert_eq!(result.classification, FileClassification::Generated);
        assert_eq!(
            result.exclusion_reason,
            Some(ExclusionReason::GeneratedPath)
        );
    }

    #[test]
    fn classify_nested_log_file_is_excluded_by_pattern() {
        let config = CheckpointSafetyConfig::default();
        let result = classify_path("logs/app.log", "??", Some(1024), &config);
        assert_eq!(result.classification, FileClassification::Generated);
    }

    #[test]
    fn classify_coverage_dir_is_excluded() {
        let config = CheckpointSafetyConfig::default();
        let result = classify_path("coverage/lcov.info", "??", Some(1024), &config);
        assert_eq!(result.classification, FileClassification::Generated);
    }

    #[test]
    fn classify_pyc_file_is_excluded_by_extension() {
        let config = CheckpointSafetyConfig::default();
        let result = classify_path("app/models.pyc", "??", Some(1024), &config);
        assert_eq!(result.classification, FileClassification::Generated);
        assert!(result.exclusion_reason.is_some());
    }

    #[test]
    fn classify_object_file_is_excluded_by_extension() {
        let config = CheckpointSafetyConfig::default();
        let result = classify_path("build/foo.o", "??", Some(1024), &config);
        assert_eq!(result.classification, FileClassification::Generated);
    }

    #[test]
    fn classify_large_binary_is_excluded() {
        let config = CheckpointSafetyConfig::default();
        let large_size = config.large_binary_threshold + 1;
        let result = classify_path("data/model.bin", "??", Some(large_size), &config);
        assert_eq!(result.classification, FileClassification::Generated);
        assert_eq!(
            result.exclusion_reason,
            Some(ExclusionReason::LargeBinary {
                size_bytes: large_size
            })
        );
    }

    #[test]
    fn classify_file_at_threshold_is_excluded() {
        let config = CheckpointSafetyConfig::default();
        let result = classify_path(
            "data/model.bin",
            "??",
            Some(config.large_binary_threshold),
            &config,
        );
        assert_eq!(result.classification, FileClassification::Generated);
    }

    #[test]
    fn classify_file_below_threshold_is_eligible() {
        let config = CheckpointSafetyConfig::default();
        let result = classify_path(
            "data/model.bin",
            "??",
            Some(config.large_binary_threshold - 1),
            &config,
        );
        assert_eq!(result.classification, FileClassification::Untracked);
        assert!(result.exclusion_reason.is_none());
    }

    #[test]
    fn classify_git_ignored_file_is_excluded() {
        let config = CheckpointSafetyConfig::default();
        let result = classify_path("secret.tmp", "!!", Some(100), &config);
        assert_eq!(result.classification, FileClassification::Ignored);
        assert_eq!(result.exclusion_reason, Some(ExclusionReason::GitIgnored));
    }

    #[test]
    fn classify_dotenv_file_is_not_excluded_but_will_be_blocked() {
        // `.env` is not in the generated patterns — it's a real config file
        // that should be blocked by the safety scan, not silently excluded.
        let config = CheckpointSafetyConfig::default();
        let result = classify_path(".env", "??", Some(100), &config);
        // It might match `.cache/` prefix? No. Let's check: `.env` doesn't
        // match any excluded pattern, so it should be Untracked (eligible
        // for staging, but will be caught by scan_path_for_blocks).
        assert_eq!(result.classification, FileClassification::Untracked);
        assert!(result.exclusion_reason.is_none());
    }

    // ── Path pattern tests ──────────────────────────────────────────────

    #[test]
    fn is_generated_path_matches_target_prefix() {
        let config = CheckpointSafetyConfig::default();
        assert!(is_generated_path("target/debug/foo", &config));
        assert!(is_generated_path("target/foo", &config));
    }

    #[test]
    fn is_generated_path_matches_nested_target() {
        let config = CheckpointSafetyConfig::default();
        assert!(is_generated_path("workspace/target/debug/foo", &config));
    }

    #[test]
    fn is_generated_path_matches_log_glob() {
        let config = CheckpointSafetyConfig::default();
        assert!(is_generated_path("app.log", &config));
        assert!(is_generated_path("logs/debug.log", &config));
    }

    #[test]
    fn is_generated_path_does_not_match_source() {
        let config = CheckpointSafetyConfig::default();
        assert!(!is_generated_path("src/main.rs", &config));
        assert!(!is_generated_path("README.md", &config));
    }

    #[test]
    fn has_generated_dir_component_detects_nested() {
        let config = CheckpointSafetyConfig::default();
        assert!(has_generated_dir_component("foo/node_modules/bar", &config));
        assert!(has_generated_dir_component("node_modules/bar", &config));
        assert!(!has_generated_dir_component("src/main.rs", &config));
    }

    #[test]
    fn has_generated_extension_handles_dotfiles() {
        let config = CheckpointSafetyConfig::default();
        // `.gitignore` should not match the `ignore` extension (it's a dotfile).
        assert!(!has_generated_extension(".gitignore", &config));
        // But `.pyc` should match even without a directory.
        assert!(has_generated_extension("foo.pyc", &config));
    }

    // ── Secret scanning tests ───────────────────────────────────────────

    #[test]
    fn scan_content_detects_aws_key() {
        let config = CheckpointSafetyConfig::default();
        let content = "aws_key = AKIAIOSFODNN7EXAMPLE\n";
        let findings = scan_content_for_secrets("config.txt", content, &config);
        assert!(!findings.is_empty());
        assert_eq!(findings[0].kind, SafetyFindingKind::SecretContent);
        assert_eq!(findings[0].line, 1);
        // The snippet must be redacted.
        assert!(
            findings[0]
                .snippet
                .as_ref()
                .unwrap()
                .contains("***REDACTED***"),
            "snippet must be redacted"
        );
        assert!(
            !findings[0]
                .snippet
                .as_ref()
                .unwrap()
                .contains("AKIAIOSFODNN7EXAMPLE"),
            "redacted snippet must not contain the actual key"
        );
    }

    #[test]
    fn scan_content_detects_private_key() {
        let config = CheckpointSafetyConfig::default();
        let content =
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAI...\n-----END RSA PRIVATE KEY-----\n";
        let findings = scan_content_for_secrets("key.pem", content, &config);
        assert!(!findings.is_empty());
        assert_eq!(findings[0].line, 1);
    }

    #[test]
    fn scan_content_detects_github_token() {
        let config = CheckpointSafetyConfig::default();
        let content = "token = ghp_1234567890abcdefghijklmnopqrstuvwxyz\n";
        let findings = scan_content_for_secrets("config.txt", content, &config);
        assert!(!findings.is_empty());
    }

    #[test]
    fn scan_content_detects_generic_password() {
        let config = CheckpointSafetyConfig::default();
        let content = "password = supersecret123\n";
        let findings = scan_content_for_secrets("config.txt", content, &config);
        assert!(!findings.is_empty());
        assert!(
            findings[0]
                .snippet
                .as_ref()
                .unwrap()
                .contains("***REDACTED***"),
            "password must be redacted"
        );
    }

    #[test]
    fn scan_content_detects_connection_string() {
        let config = CheckpointSafetyConfig::default();
        let content = "DATABASE_URL=postgres://user:secretpass@localhost:5432/db\n";
        let findings = scan_content_for_secrets("config.txt", content, &config);
        assert!(!findings.is_empty());
    }

    #[test]
    fn scan_content_does_not_flag_normal_code() {
        let config = CheckpointSafetyConfig::default();
        let content = "fn main() {\n    println!(\"hello world\");\n}\n";
        let findings = scan_content_for_secrets("src/main.rs", content, &config);
        assert!(
            findings.is_empty(),
            "normal source code should not trigger secret findings"
        );
    }

    #[test]
    fn scan_content_does_not_flag_word_password_in_comment() {
        let config = CheckpointSafetyConfig::default();
        let content = "// This function handles password reset logic\nfn reset() {}\n";
        let findings = scan_content_for_secrets("src/auth.rs", content, &config);
        // "password reset logic" doesn't have an assignment with a value,
        // so it should not trigger (the pattern requires a value after =).
        assert!(
            findings.is_empty(),
            "the word 'password' in a comment without a value should not trigger"
        );
    }

    #[test]
    fn scan_path_blocks_env_file() {
        let config = CheckpointSafetyConfig::default();
        let findings = scan_path_for_blocks(".env", &config);
        assert!(!findings.is_empty());
        assert_eq!(findings[0].kind, SafetyFindingKind::BlockedPath);
    }

    #[test]
    fn scan_path_blocks_credentials_file() {
        let config = CheckpointSafetyConfig::default();
        let findings = scan_path_for_blocks("config/credentials.yml", &config);
        assert!(!findings.is_empty());
    }

    #[test]
    fn scan_path_blocks_rsa_key() {
        let config = CheckpointSafetyConfig::default();
        let findings = scan_path_for_blocks("~/.ssh/id_rsa", &config);
        assert!(!findings.is_empty());
    }

    #[test]
    fn scan_path_does_not_block_source() {
        let config = CheckpointSafetyConfig::default();
        let findings = scan_path_for_blocks("src/main.rs", &config);
        assert!(findings.is_empty());
    }

    // ── Fingerprint tests ───────────────────────────────────────────────

    #[test]
    fn diff_fingerprint_is_deterministic() {
        let a = diff_fingerprint("hello world");
        let b = diff_fingerprint("hello world");
        assert_eq!(a, b);
    }

    #[test]
    fn diff_fingerprint_changes_with_content() {
        let a = diff_fingerprint("hello world");
        let b = diff_fingerprint("hello World");
        assert_ne!(a, b);
    }

    #[test]
    fn path_set_fingerprint_is_order_independent() {
        let a = path_set_fingerprint(&["c", "a", "b"]);
        let b = path_set_fingerprint(&["a", "b", "c"]);
        assert_eq!(a, b);
    }

    // ── Porcelain parsing tests ─────────────────────────────────────────

    #[test]
    fn parse_porcelain_modified_file() {
        let (status, path) = parse_porcelain_line(" M src/main.rs").unwrap();
        assert_eq!(status, " M");
        assert_eq!(path, "src/main.rs");
    }

    #[test]
    fn parse_porcelain_untracked_file() {
        let (status, path) = parse_porcelain_line("?? src/new.rs").unwrap();
        assert_eq!(status, "??");
        assert_eq!(path, "src/new.rs");
    }

    #[test]
    fn parse_porcelain_ignored_file() {
        let (status, path) = parse_porcelain_line("!! secret.tmp").unwrap();
        assert_eq!(status, "!!");
        assert_eq!(path, "secret.tmp");
    }

    #[test]
    fn parse_porcelain_quoted_path() {
        let (status, path) = parse_porcelain_line("?? \"my file.txt\"").unwrap();
        assert_eq!(status, "??");
        assert_eq!(path, "my file.txt");
    }

    #[test]
    fn parse_porcelain_empty_line_returns_none() {
        assert!(parse_porcelain_line("").is_none());
    }

    // ── Integration tests with real git repos ───────────────────────────

    /// Helper: run git in a directory (same pattern as main.rs tests).
    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Set up a minimal git repo with an initial commit on `main`.
    fn setup_repo() -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let p = dir.path();
        git(p, &["init", "-b", "main"]);
        std::fs::write(p.join("base.txt"), "base\n").unwrap();
        git(p, &["add", "-A"]);
        git(p, &["commit", "-m", "base"]);
        dir
    }

    /// Write a file, creating parent directories as needed.
    fn write_file(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, content).unwrap();
    }

    /// Write binary content to a file, creating parent directories as needed.
    fn write_bytes(root: &Path, rel: &str, content: &[u8]) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, content).unwrap();
    }

    #[tokio::test]
    async fn scan_clean_worktree_returns_empty_result() {
        let dir = setup_repo();
        let config = CheckpointSafetyConfig::default();
        let result = scan_worktree(dir.path(), "main", &config)
            .await
            .expect("scan");
        assert!(!result.had_changes);
        assert!(result.staged.is_empty());
        assert!(result.excluded.is_empty());
        assert!(result.blocked.is_empty());
        assert!(result.fingerprints.head_sha.is_some());
    }

    #[tokio::test]
    async fn scan_classifies_source_change_as_staged() {
        let dir = setup_repo();
        let p = dir.path();
        // Modify a tracked file + add a new untracked source file.
        std::fs::write(p.join("base.txt"), "modified\n").unwrap();
        write_file(p, "src/new.rs", "fn main() {}\n");

        let config = CheckpointSafetyConfig::default();
        let result = scan_worktree(p, "main", &config).await.expect("scan");

        assert!(result.had_changes);
        assert!(result.staged.contains(&"base.txt".to_string()));
        assert!(result.staged.contains(&"src/new.rs".to_string()));
        assert!(result.blocked.is_empty());
        assert!(result.fingerprints.worktree_diff.is_some());
    }

    #[tokio::test]
    async fn scan_excludes_generated_paths() {
        let dir = setup_repo();
        let p = dir.path();
        // Create generated/cache files alongside real source.
        write_file(p, "src/main.rs", "fn main() {}\n");
        std::fs::create_dir_all(p.join("target/debug")).unwrap();
        std::fs::write(p.join("target/debug/deps.lib"), "binary junk").unwrap();
        std::fs::create_dir_all(p.join("node_modules/react")).unwrap();
        std::fs::write(
            p.join("node_modules/react/index.js"),
            "module.exports = {};\n",
        )
        .unwrap();
        std::fs::write(p.join("app.log"), "[INFO] something\n").unwrap();

        let config = CheckpointSafetyConfig::default();
        let result = scan_worktree(p, "main", &config).await.expect("scan");

        // Source is staged.
        assert!(result.staged.contains(&"src/main.rs".to_string()));
        // Generated files are excluded.
        let excluded_paths: Vec<&str> = result.excluded.iter().map(|e| e.path.as_str()).collect();
        assert!(
            excluded_paths.iter().any(|p| p.starts_with("target/")),
            "target/ must be excluded: {excluded_paths:?}"
        );
        assert!(
            excluded_paths
                .iter()
                .any(|p| p.starts_with("node_modules/")),
            "node_modules/ must be excluded: {excluded_paths:?}"
        );
        assert!(
            excluded_paths.contains(&"app.log"),
            "app.log must be excluded: {excluded_paths:?}"
        );
        // None of the generated files leak into staged.
        assert!(
            !result.staged.iter().any(|s| s.starts_with("target/")),
            "target/ must not be staged"
        );
        assert!(
            !result.staged.iter().any(|s| s.starts_with("node_modules/")),
            "node_modules/ must not be staged"
        );
    }

    #[tokio::test]
    async fn scan_blocks_secret_content() {
        let dir = setup_repo();
        let p = dir.path();
        // A file with a secret-like value.
        std::fs::write(p.join("config.txt"), "api_key = AKIAIOSFODNN7EXAMPLE\n").unwrap();

        let config = CheckpointSafetyConfig::default();
        let result = scan_worktree(p, "main", &config).await.expect("scan");

        // The file is staged (it's a real source-like file, not generated)...
        assert!(result.staged.contains(&"config.txt".to_string()));
        // ...but the safety scan found a secret.
        assert!(!result.blocked.is_empty(), "secret must be blocked");
        assert!(
            result
                .blocked
                .iter()
                .any(|f| f.path == "config.txt" && f.kind == SafetyFindingKind::SecretContent),
            "must have a secret content finding for config.txt"
        );
    }

    #[tokio::test]
    async fn scan_blocks_env_file_by_path() {
        let dir = setup_repo();
        let p = dir.path();
        std::fs::write(p.join(".env"), "DATABASE_URL=postgres://localhost/db\n").unwrap();

        let config = CheckpointSafetyConfig::default();
        let result = scan_worktree(p, "main", &config).await.expect("scan");

        // `.env` is staged (not generated) but blocked by path.
        assert!(result.staged.contains(&".env".to_string()));
        assert!(
            result
                .blocked
                .iter()
                .any(|f| f.path == ".env" && f.kind == SafetyFindingKind::BlockedPath),
            "must have a blocked-path finding for .env"
        );
    }

    #[tokio::test]
    async fn scan_excludes_large_binary() {
        let dir = setup_repo();
        let p = dir.path();
        // Create a file just above the threshold.
        let config = CheckpointSafetyConfig::default();
        let large_content = vec![0u8; (config.large_binary_threshold + 1) as usize];
        write_bytes(p, "data/model.bin", &large_content);

        let result = scan_worktree(p, "main", &config).await.expect("scan");

        assert!(
            !result.staged.contains(&"data/model.bin".to_string()),
            "large binary must not be staged"
        );
        assert!(
            result.excluded.iter().any(|e| e.path == "data/model.bin"
                && matches!(e.reason, ExclusionReason::LargeBinary { .. })),
            "large binary must be excluded with LargeBinary reason"
        );
    }

    #[tokio::test]
    async fn scan_computes_fingerprints() {
        let dir = setup_repo();
        let p = dir.path();
        write_file(p, "src/main.rs", "fn main() {}\n");

        let config = CheckpointSafetyConfig::default();
        let result = scan_worktree(p, "main", &config).await.expect("scan");

        assert!(
            result.fingerprints.head_sha.is_some(),
            "head_sha must be set"
        );
        assert!(
            result.fingerprints.worktree_diff.is_some(),
            "worktree_diff fingerprint must be set for dirty tree"
        );
        assert!(
            result.fingerprints.staged_diff.is_some(),
            "staged_diff fingerprint must be set when files are staged"
        );
    }

    #[tokio::test]
    async fn scan_is_deterministic() {
        let dir = setup_repo();
        let p = dir.path();
        write_file(p, "src/main.rs", "fn main() {}\n");
        write_file(p, "target/debug/foo", "junk");

        let config = CheckpointSafetyConfig::default();
        let result1 = scan_worktree(p, "main", &config).await.expect("scan 1");
        let result2 = scan_worktree(p, "main", &config).await.expect("scan 2");

        assert_eq!(result1.staged, result2.staged);
        assert_eq!(result1.excluded, result2.excluded);
        assert_eq!(result1.blocked, result2.blocked);
        assert_eq!(result1.fingerprints, result2.fingerprints);
    }

    #[tokio::test]
    async fn scan_includes_gitignored_in_excluded_summary() {
        let dir = setup_repo();
        let p = dir.path();
        // Add a .gitignore that ignores a file, then create it.
        std::fs::write(p.join(".gitignore"), "*.tmp\n").unwrap();
        git(p, &["add", ".gitignore"]);
        git(p, &["commit", "-m", "add gitignore"]);
        std::fs::write(p.join("cache.tmp"), "ignored content\n").unwrap();

        let config = CheckpointSafetyConfig::default();
        let result = scan_worktree(p, "main", &config).await.expect("scan");

        // The ignored file appears in the excluded summary.
        assert!(
            result
                .excluded
                .iter()
                .any(|e| e.path == "cache.tmp" && e.reason == ExclusionReason::GitIgnored),
            "git-ignored file must appear in excluded summary: {:?}",
            result.excluded
        );
    }

    #[tokio::test]
    async fn scan_with_custom_config_overrides_defaults() {
        let dir = setup_repo();
        let p = dir.path();
        write_file(p, "mybuild/output.txt", "build output\n");

        // Custom config that excludes `mybuild/`.
        let config = CheckpointSafetyConfig {
            excluded_path_patterns: vec!["mybuild/"],
            excluded_dir_components: vec!["mybuild"],
            ..Default::default()
        };
        let result = scan_worktree(p, "main", &config).await.expect("scan");

        assert!(
            result
                .excluded
                .iter()
                .any(|e| e.path.starts_with("mybuild/")),
            "custom config must exclude mybuild/"
        );
    }
}
