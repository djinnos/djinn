//! Verification-input fingerprint (V1).
//!
//! Deterministic, byte-oriented canonical stream over the complete repository
//! input state. This is **distinct** from submission integrity
//! ([`crate::compute_submission_diff_fingerprint`]) and must never call or
//! alter it.
//!
//! Every variable-length field is length-delimited (u64 LE prefix + bytes) so
//! the framing is unambiguous and stable across versions. Paths are sorted
//! bytewise. Unsupported/special entries (FIFOs, sockets, devices, gitlinks)
//! cause identity to become **unavailable** rather than being silently skipped,
//! so downstream execution never fabricates a reusable identity from an
//! incomplete hash.

use std::path::{Path, PathBuf};

use djinn_core::canonical_verify::{
    VERIFICATION_INPUT_MANIFEST_VERSION_V1, VerificationInputManifestV1,
};
use globset::{Glob, GlobSet, GlobSetBuilder};
use sha2::{Digest, Sha256};

use crate::{CommandOutput, GitError, run_git_command_allow_failure, run_git_command_binary_in};

// ─── Constants ──────────────────────────────────────────────────────────────

/// Canonical-stream version number produced by this implementation.
pub const VERIFICATION_INPUT_FINGERPRINT_VERSION_V1: u32 = 1;

/// Default base ref used when fingerprinting verification input.
///
/// Task worktrees normally branch from the project's target branch, which
/// defaults to `main` when no project-specific target branch is configured.
pub const DEFAULT_VERIFICATION_BASE_REF: &str = "main";

/// Magic string written at the start of every V1 canonical stream.
const STREAM_MAGIC: &[u8] = b"djinn-verification-input-fingerprint";

/// Version tag written immediately after the magic string.
const STREAM_VERSION_TAG: &[u8] = b"v1";

// Worktree entry type tags (byte literals for stable framing).
const TYPE_REGULAR: &[u8] = b"regular";
const TYPE_SYMLINK: &[u8] = b"symlink";
const TYPE_MISSING: &[u8] = b"missing";

// Worktree entry mode tags.
const MODE_EXEC: &[u8] = b"exec";
const MODE_NORMAL: &[u8] = b"normal";

// ─── Configuration ──────────────────────────────────────────────────────────

/// Configuration for computing a verification-input fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationInputFingerprintConfig {
    /// Base branch/ref used to find the merge-base via
    /// `git merge-base <base_ref> HEAD`.
    pub base_ref: String,
    /// Resolved V1 declarations controlling external inputs and output cleanup.
    pub manifest: VerificationInputManifestV1,
    /// Concrete mounts corresponding one-to-one with logical declarations.
    pub external_inputs: Vec<ResolvedExternalInputV1>,
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

/// Concrete filesystem mount for one logical external input declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedExternalInputV1 {
    pub id: String,
    pub path: PathBuf,
}

fn empty_manifest() -> VerificationInputManifestV1 {
    VerificationInputManifestV1 {
        version: VERIFICATION_INPUT_MANIFEST_VERSION_V1,
        repo_paths: Vec::new(),
        environment_names: Vec::new(),
        read_only_external_inputs: Vec::new(),
        output_only_globs: Vec::new(),
    }
}

impl Default for VerificationInputFingerprintConfig {
    fn default() -> Self {
        Self {
            base_ref: DEFAULT_VERIFICATION_BASE_REF.to_string(),
            manifest: empty_manifest(),
            external_inputs: Vec::new(),
        }
    }
}

impl VerificationInputFingerprintConfig {
    pub fn new(base_ref: impl Into<String>) -> Self {
        Self {
            base_ref: base_ref.into(),
            ..Self::default()
        }
    }
}

// ─── Result types ───────────────────────────────────────────────────────────

/// Result of computing a verification-input fingerprint.
///
/// The [`Available`](Self::Available) variant carries a stable V1 digest that
/// is safe to use for reusable verification. The
/// [`Unavailable`](Self::Unavailable) variant carries a stable reason why no
/// identity could be established — downstream code must treat this as
/// "identity unknown, normal verification required" and must never fabricate
/// an identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerificationInputFingerprint {
    /// A V1 digest was successfully computed.
    Available(VerificationInputDigestV1),
    /// No stable identity could be established; no reusable row should be
    /// written.
    Unavailable(VerificationInputUnavailable),
}

impl VerificationInputFingerprint {
    /// `true` when a V1 digest is available.
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available(_))
    }

    /// `true` when identity is unavailable.
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }

    /// The 64-character lowercase hex SHA-256 digest, or `None` when
    /// unavailable.
    pub fn fingerprint(&self) -> Option<&str> {
        match self {
            Self::Available(digest) => Some(&digest.fingerprint),
            Self::Unavailable(_) => None,
        }
    }

    /// The unavailable reason, or `None` when a digest is available.
    pub fn unavailable_reason(&self) -> Option<&VerificationInputUnavailable> {
        match self {
            Self::Available(_) => None,
            Self::Unavailable(reason) => Some(reason),
        }
    }
}

/// A successfully computed V1 verification-input digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationInputDigestV1 {
    /// Canonical-stream version (always
    /// [`VERIFICATION_INPUT_FINGERPRINT_VERSION_V1`]).
    pub version: u32,
    /// 64-character lowercase hex SHA-256 of the canonical byte stream.
    pub fingerprint: String,
    /// Length of the canonical byte stream in bytes.
    pub canonical_stream_len: u64,
    /// Resolved merge-base SHA, when one was computed.
    pub merge_base: Option<String>,
    /// Resolved HEAD SHA.
    pub head: String,
    /// Number of tracked index/worktree entries hashed.
    pub tracked_entry_count: u64,
    /// Number of untracked + ignored worktree entries hashed.
    pub extra_entry_count: u64,
}

/// Stable fail-closed reason a verification-input identity is unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerificationInputUnavailable {
    /// The configured base ref could not be resolved, so no merge-base exists.
    UnresolvedBaseRef {
        /// The base ref that was attempted (e.g. `"main"` or `"origin/main"`).
        base_ref: String,
    },
    /// V1 declarations or their resolved mounts were unsafe or ambiguous.
    MalformedManifest { detail: String },
    /// A logical external declaration has no usable resolved mount.
    MissingExternalInput { id: String },
    /// HEAD could not be resolved (e.g. a repository with no commits).
    UnresolvedHead,
    /// An index entry has an unsupported mode (e.g. gitlink `160000` for
    /// submodules, which are handled in a sibling task).
    UnsupportedIndexMode {
        /// Repository-relative path of the entry.
        path: String,
        /// The raw mode string from `git ls-files -s` (e.g. `"160000"`).
        mode: String,
    },
    /// A worktree entry is a special file type (FIFO, socket, device, etc.)
    /// that cannot be hashed deterministically.
    UnsupportedSpecialFile {
        /// Repository-relative path of the entry.
        path: String,
        /// Human-readable kind label (e.g. `"fifo"`, `"socket"`).
        kind: String,
    },
    /// A worktree file exists but could not be read (e.g. permission denied).
    UnreadableFile {
        /// Repository-relative path of the file.
        path: String,
        /// Lowercased error message from the failed read.
        error: String,
    },
    /// An untracked or ignored entry listed by `git ls-files` vanished before
    /// it could be read (a traversal race).
    MissingExtraEntry {
        /// Repository-relative path of the vanished entry.
        path: String,
    },
}

impl std::fmt::Display for VerificationInputUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnresolvedBaseRef { base_ref } => {
                write!(
                    f,
                    "verification input unavailable: unresolved base ref {base_ref}"
                )
            }
            Self::MalformedManifest { detail } => {
                write!(
                    f,
                    "verification input unavailable: malformed manifest: {detail}"
                )
            }
            Self::MissingExternalInput { id } => {
                write!(
                    f,
                    "verification input unavailable: missing external input {id}"
                )
            }
            Self::UnresolvedHead => {
                write!(f, "verification input unavailable: unresolved HEAD")
            }
            Self::UnsupportedIndexMode { path, mode } => {
                write!(
                    f,
                    "verification input unavailable: unsupported index mode {mode} for {path}"
                )
            }
            Self::UnsupportedSpecialFile { path, kind } => {
                write!(
                    f,
                    "verification input unavailable: unsupported {kind} at {path}"
                )
            }
            Self::UnreadableFile { path, error } => {
                write!(
                    f,
                    "verification input unavailable: unreadable file {path}: {error}"
                )
            }
            Self::MissingExtraEntry { path } => {
                write!(
                    f,
                    "verification input unavailable: missing extra entry {path}"
                )
            }
        }
    }
}

/// Infrastructure-level fingerprint computation error.
#[derive(Debug, thiserror::Error)]
pub enum VerificationInputError {
    /// A git command could not be spawned or exited with an unexpected failure.
    #[error("git command failed: {0}")]
    Git(#[from] GitError),
}

// ─── Public API ─────────────────────────────────────────────────────────────

/// Compute a verification-input fingerprint using the default base ref
/// ([`DEFAULT_VERIFICATION_BASE_REF`]).
pub async fn compute_verification_input_fingerprint(
    worktree: impl AsRef<Path>,
) -> Result<VerificationInputFingerprint, VerificationInputError> {
    compute_verification_input_fingerprint_with_config(
        worktree,
        &VerificationInputFingerprintConfig::default(),
    )
    .await
}

/// Compute a verification-input fingerprint with an explicit configuration.
///
/// The canonical stream is built over the resolved merge-base and HEAD, index
/// entries, tracked worktree state, and every untracked/ignored worktree entry.
/// Paths are sorted bytewise and unsupported/special entries cause identity to
/// become unavailable. This function never calls
/// [`compute_submission_diff_fingerprint`](crate::compute_submission_diff_fingerprint).
pub async fn compute_verification_input_fingerprint_with_config(
    worktree: impl AsRef<Path>,
    config: &VerificationInputFingerprintConfig,
) -> Result<VerificationInputFingerprint, VerificationInputError> {
    let worktree = worktree.as_ref();
    let output_only = match validate_manifest(config) {
        Ok(globs) => globs,
        Err(reason) => return Ok(VerificationInputFingerprint::Unavailable(reason)),
    };
    if let Err(reason) = cleanup_output_only(worktree, &output_only) {
        return Ok(VerificationInputFingerprint::Unavailable(reason));
    }
    let base_ref = config.base_ref.trim();
    let base_ref = if base_ref.is_empty() {
        DEFAULT_VERIFICATION_BASE_REF
    } else {
        base_ref
    };

    // ── Resolve HEAD ────────────────────────────────────────────────────────
    let head = match try_rev_parse(worktree, "HEAD").await? {
        Some(sha) => sha,
        None => {
            return Ok(VerificationInputFingerprint::Unavailable(
                VerificationInputUnavailable::UnresolvedHead,
            ));
        }
    };

    // ── Resolve base ref and merge-base ─────────────────────────────────────
    let resolved_base_ref = match resolve_base_ref(worktree, base_ref).await? {
        Some(r) => r,
        None => {
            return Ok(VerificationInputFingerprint::Unavailable(
                VerificationInputUnavailable::UnresolvedBaseRef {
                    base_ref: base_ref.to_string(),
                },
            ));
        }
    };
    let merge_base = match try_merge_base(worktree, &resolved_base_ref).await? {
        Some(mb) => mb,
        None => {
            return Ok(VerificationInputFingerprint::Unavailable(
                VerificationInputUnavailable::UnresolvedBaseRef {
                    base_ref: resolved_base_ref,
                },
            ));
        }
    };

    // ── Collect index entries ───────────────────────────────────────────────
    let index_output =
        git_binary_stdout(worktree, vec!["ls-files".into(), "-s".into(), "-z".into()]).await?;
    let mut index_entries = parse_index_entries(&index_output);
    index_entries.retain(|entry| !output_only.is_match(path_from_bytes(&entry.path)));

    // Validate index modes before hashing.
    for entry in &index_entries {
        if !is_supported_index_mode(&entry.mode) {
            return Ok(VerificationInputFingerprint::Unavailable(
                VerificationInputUnavailable::UnsupportedIndexMode {
                    path: lossy_path(&entry.path),
                    mode: lossy_path(&entry.mode),
                },
            ));
        }
    }

    // ── Collect tracked worktree states ─────────────────────────────────────
    let mut tracked_states = Vec::with_capacity(index_entries.len());
    for entry in &index_entries {
        match classify_worktree_entry(worktree, &entry.path, false) {
            Ok(state) => tracked_states.push(state),
            Err(unavailable) => {
                return Ok(VerificationInputFingerprint::Unavailable(unavailable));
            }
        }
    }

    // ── Collect extra (untracked + ignored) entries ─────────────────────────
    let mut extra_paths = collect_extra_paths(worktree).await?;
    extra_paths.retain(|path| !output_only.is_match(path_from_bytes(path)));
    let mut extra_states = Vec::with_capacity(extra_paths.len());
    for path in &extra_paths {
        match classify_worktree_entry(worktree, path, true) {
            Ok(state) => extra_states.push(state),
            Err(unavailable) => {
                return Ok(VerificationInputFingerprint::Unavailable(unavailable));
            }
        }
    }

    // ── Build canonical stream ──────────────────────────────────────────────
    index_entries.sort_by(|a, b| a.path.cmp(&b.path));
    tracked_states.sort_by(|a, b| a.path.cmp(&b.path));
    extra_states.sort_by(|a, b| a.path.cmp(&b.path));
    let external_states = match collect_external_states(config) {
        Ok(states) => states,
        Err(reason) => return Ok(VerificationInputFingerprint::Unavailable(reason)),
    };

    let tracked_count = tracked_states.len() as u64;
    let extra_count = extra_states.len() as u64;

    let mut stream = CanonicalStream::new();
    stream.write_header();
    stream.write_refs(&merge_base, &head);
    stream.write_index_entries(&index_entries);
    stream.write_worktree_states(&tracked_states);
    stream.write_worktree_states(&extra_states);
    stream.write_external_states(&external_states);
    let canonical_bytes = stream.finalize();
    let canonical_stream_len = canonical_bytes.len() as u64;

    let fingerprint = sha256_hex(&canonical_bytes);

    Ok(VerificationInputFingerprint::Available(
        VerificationInputDigestV1 {
            version: VERIFICATION_INPUT_FINGERPRINT_VERSION_V1,
            fingerprint,
            canonical_stream_len,
            merge_base: Some(merge_base),
            head,
            tracked_entry_count: tracked_count,
            extra_entry_count: extra_count,
        },
    ))
}

fn unavailable_manifest(detail: impl Into<String>) -> VerificationInputUnavailable {
    VerificationInputUnavailable::MalformedManifest {
        detail: detail.into(),
    }
}

fn validate_manifest(
    config: &VerificationInputFingerprintConfig,
) -> Result<GlobSet, VerificationInputUnavailable> {
    let manifest = &config.manifest;
    if manifest.version != VERIFICATION_INPUT_MANIFEST_VERSION_V1 {
        return Err(unavailable_manifest("unsupported manifest version"));
    }
    let mut repo_paths = std::collections::BTreeSet::new();
    for path in &manifest.repo_paths {
        if !safe_relative(path) || !repo_paths.insert(path) {
            return Err(unavailable_manifest("invalid or duplicate repo input path"));
        }
    }
    let mut declared = std::collections::BTreeSet::new();
    for input in &manifest.read_only_external_inputs {
        if input.id.trim().is_empty()
            || input.locator.trim().is_empty()
            || !declared.insert(input.id.as_str())
        {
            return Err(unavailable_manifest("ambiguous external input declaration"));
        }
    }
    if config.external_inputs.len() != manifest.read_only_external_inputs.len() {
        return Err(unavailable_manifest(
            "external declarations do not match resolved mounts",
        ));
    }
    let mut resolved = std::collections::BTreeSet::new();
    for mount in &config.external_inputs {
        if mount.id.trim().is_empty()
            || mount.path.as_os_str().is_empty()
            || !declared.contains(mount.id.as_str())
            || !resolved.insert(mount.id.as_str())
        {
            return Err(unavailable_manifest(
                "undeclared or ambiguous external mount",
            ));
        }
    }
    let mut builder = GlobSetBuilder::new();
    let mut outputs = std::collections::BTreeSet::new();
    for pattern in &manifest.output_only_globs {
        if !safe_relative(pattern) || !outputs.insert(pattern) {
            return Err(unavailable_manifest(
                "invalid or ambiguous output-only glob",
            ));
        }
        let glob =
            Glob::new(pattern).map_err(|_| unavailable_manifest("invalid output-only glob"))?;
        let matcher = glob.compile_matcher();
        if repo_paths.iter().any(|path| matcher.is_match(path))
            || matcher.is_match(".git")
            || matcher.is_match(".git/config")
        {
            return Err(unavailable_manifest(
                "output-only glob overlaps declared input or repository metadata",
            ));
        }
        builder.add(glob);
    }
    let output_patterns: Vec<_> = outputs.into_iter().collect();
    for (index, pattern) in output_patterns.iter().enumerate() {
        if output_patterns[index + 1..]
            .iter()
            .any(|other| output_globs_may_overlap(pattern, other))
        {
            return Err(unavailable_manifest(
                "invalid or ambiguous output-only glob",
            ));
        }
    }
    builder
        .build()
        .map_err(|_| unavailable_manifest("invalid output-only glob"))
}

/// Conservatively determine whether two output patterns can select the same
/// path. A false positive rejects an unnecessarily broad manifest, while a
/// false negative could delete input state, so wildcard components are treated
/// as overlapping unless distinct literal components prove otherwise.
fn output_globs_may_overlap(left: &str, right: &str) -> bool {
    fn overlap(left: &[&str], right: &[&str]) -> bool {
        match (left.split_first(), right.split_first()) {
            (None, None) => true,
            (Some((left_component, left_rest)), _) if *left_component == "**" => {
                overlap(left_rest, right)
                    || right
                        .split_first()
                        .is_some_and(|(_, right_rest)| overlap(left, right_rest))
            }
            (_, Some((right_component, right_rest))) if *right_component == "**" => {
                overlap(left, right_rest)
                    || left
                        .split_first()
                        .is_some_and(|(_, left_rest)| overlap(left_rest, right))
            }
            (Some((left_component, left_rest)), Some((right_component, right_rest))) => {
                if left_component != right_component
                    && !glob_component(left_component)
                    && !glob_component(right_component)
                {
                    false
                } else {
                    overlap(left_rest, right_rest)
                }
            }
            _ => false,
        }
    }

    overlap(
        &left.split('/').collect::<Vec<_>>(),
        &right.split('/').collect::<Vec<_>>(),
    )
}

fn glob_component(component: &str) -> bool {
    component.contains(['*', '?', '[', '{'])
}

fn safe_relative(value: &str) -> bool {
    !value.is_empty()
        && !Path::new(value).is_absolute()
        && !value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
}

fn cleanup_output_only(
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

struct ExternalState {
    id: Vec<u8>,
    locator: Vec<u8>,
    path: Vec<u8>,
    state: WorktreeState,
}

fn collect_external_states(
    config: &VerificationInputFingerprintConfig,
) -> Result<Vec<ExternalState>, VerificationInputUnavailable> {
    let mut states = Vec::new();
    for declaration in &config.manifest.read_only_external_inputs {
        let mount = config
            .external_inputs
            .iter()
            .find(|mount| mount.id == declaration.id)
            .ok_or_else(|| VerificationInputUnavailable::MissingExternalInput {
                id: declaration.id.clone(),
            })?;
        if !std::fs::symlink_metadata(&mount.path)
            .map(|m| m.file_type().is_dir())
            .unwrap_or(false)
        {
            return Err(VerificationInputUnavailable::MissingExternalInput {
                id: declaration.id.clone(),
            });
        }
        collect_external_dir(&mount.path, &mount.path, declaration, &mut states)?;
    }
    states.sort_by(|a, b| (&a.id, &a.locator, &a.path).cmp(&(&b.id, &b.locator, &b.path)));
    Ok(states)
}

fn collect_external_dir(
    root: &Path,
    dir: &Path,
    declaration: &djinn_core::canonical_verify::DeclaredExternalInputV1,
    states: &mut Vec<ExternalState>,
) -> Result<(), VerificationInputUnavailable> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| VerificationInputUnavailable::UnreadableFile {
            path: dir.display().to_string(),
            error: e.to_string(),
        })?
        .collect::<Result<_, _>>()
        .map_err(|e| VerificationInputUnavailable::UnreadableFile {
            path: dir.display().to_string(),
            error: e.to_string(),
        })?;
    entries.sort_by_key(|entry| {
        let name = entry.file_name();
        path_bytes(Path::new(&name))
    });
    for entry in entries {
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .map_err(|_| unavailable_manifest("external path escaped mount"))?;
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|_| unavailable_manifest("external traversal changed"))?;
        if meta.file_type().is_dir() {
            collect_external_dir(root, &path, declaration, states)?;
        } else {
            let bytes = path_bytes(rel);
            states.push(ExternalState {
                id: declaration.id.as_bytes().to_vec(),
                locator: declaration.locator.as_bytes().to_vec(),
                path: bytes.clone(),
                state: classify_worktree_entry(root, &bytes, false)?,
            });
        }
    }
    Ok(())
}

// ─── Internal: git command helpers ──────────────────────────────────────────

async fn git_binary_stdout(worktree: &Path, args: Vec<String>) -> Result<Vec<u8>, GitError> {
    let worktree = worktree.to_path_buf();
    tokio::task::spawn_blocking(move || {
        run_git_command_binary_in(&worktree, args).map(|output| output.stdout)
    })
    .await
    .map_err(|e| GitError::Io(std::io::Error::other(e.to_string())))?
}

async fn git_allow_failure(worktree: &Path, args: Vec<String>) -> Result<CommandOutput, GitError> {
    run_git_command_allow_failure(PathBuf::from(worktree), args).await
}

/// Resolve a ref via `git rev-parse --verify <ref>`. Returns `Ok(Some(sha))`
/// when the ref exists, `Ok(None)` when it does not (non-zero exit), or
/// `Err` on a spawn/IO failure.
async fn try_rev_parse(
    worktree: &Path,
    rev: &str,
) -> Result<Option<String>, VerificationInputError> {
    let result = git_allow_failure(
        worktree,
        vec!["rev-parse".into(), "--verify".into(), rev.into()],
    )
    .await?;
    if result.is_success() {
        let sha = result.stdout.trim().to_string();
        if sha.is_empty() {
            Ok(None)
        } else {
            Ok(Some(sha))
        }
    } else {
        Ok(None)
    }
}

/// Resolve a base ref name, trying the bare name first then `origin/<name>`.
async fn resolve_base_ref(
    worktree: &Path,
    base_ref: &str,
) -> Result<Option<String>, VerificationInputError> {
    if try_rev_parse(worktree, base_ref).await?.is_some() {
        return Ok(Some(base_ref.to_string()));
    }
    let origin_ref = format!("origin/{base_ref}");
    if try_rev_parse(worktree, &origin_ref).await?.is_some() {
        return Ok(Some(origin_ref));
    }
    Ok(None)
}

/// Compute `git merge-base <base> HEAD`. Returns `Ok(Some(sha))` on success,
/// `Ok(None)` when the merge-base does not exist.
async fn try_merge_base(
    worktree: &Path,
    base: &str,
) -> Result<Option<String>, VerificationInputError> {
    let result = git_allow_failure(
        worktree,
        vec!["merge-base".into(), base.into(), "HEAD".into()],
    )
    .await?;
    if result.is_success() {
        let sha = result.stdout.trim().to_string();
        if sha.is_empty() {
            Ok(None)
        } else {
            Ok(Some(sha))
        }
    } else {
        Ok(None)
    }
}

// ─── Internal: index entry parsing ──────────────────────────────────────────

/// One entry from `git ls-files -s -z`.
#[derive(Debug, Clone)]
struct IndexEntry {
    /// Repository-relative path as raw bytes (not lossy-decoded).
    path: Vec<u8>,
    /// Raw mode string, e.g. `b"100644"`, `b"100755"`, `b"120000"`, `b"160000"`.
    mode: Vec<u8>,
    /// Index stage (0 = normal, nonzero = conflict).
    stage: u32,
    /// 40-character blob SHA hex.
    blob_sha: String,
}

/// Parse the output of `git ls-files -s -z` into structured entries.
///
/// Each NUL-delimited record has the format:
/// `"<mode> <sha> <stage>\t<path>"`
///
/// Operates on raw bytes so that non-UTF-8 paths are preserved exactly as Git
/// emitted them — never lossy-decoded.
fn parse_index_entries(output: &[u8]) -> Vec<IndexEntry> {
    let mut entries = Vec::new();
    for record in output.split(|&b| b == 0) {
        if record.is_empty() {
            continue;
        }
        // The tab separates metadata from the path.
        let Some(tab_pos) = record.iter().position(|&b| b == b'\t') else {
            continue;
        };
        let metadata = &record[..tab_pos];
        let path = &record[tab_pos + 1..];

        // Split metadata into three space-delimited fields: mode, sha, stage.
        let mut parts = metadata.splitn(3, |&b| b == b' ');
        let Some(mode) = parts.next() else { continue };
        let Some(blob_sha) = parts.next() else {
            continue;
        };
        let Some(stage) = parts.next() else { continue };

        let blob_sha = std::str::from_utf8(blob_sha).unwrap_or("").to_string();
        let stage = std::str::from_utf8(stage)
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);
        entries.push(IndexEntry {
            path: path.to_vec(),
            mode: mode.to_vec(),
            stage,
            blob_sha,
        });
    }
    entries
}

/// `true` for supported blob/symlink index modes.
///
/// Regular blobs: `100644` (normal) and `100755` (executable).
/// Symlinks: `120000`.
/// Gitlinks/submodules (`160000`) and tree modes (`040000`) are unsupported
/// in V1 — they cause identity to become unavailable.
fn is_supported_index_mode(mode: &[u8]) -> bool {
    matches!(mode, b"100644" | b"100755" | b"120000")
}

// ─── Internal: worktree entry classification ────────────────────────────────

/// Classified state of a single worktree entry for the canonical stream.
#[derive(Debug, Clone)]
struct WorktreeState {
    /// Repository-relative path as raw bytes (not lossy-decoded).
    path: Vec<u8>,
    type_tag: &'static [u8],
    mode_tag: &'static [u8],
    /// File content for regular files, symlink target bytes for symlinks,
    /// empty for missing entries.
    content: Vec<u8>,
}

/// Convert raw path bytes into a platform path component without lossy UTF-8
/// conversion.
///
/// On Unix, paths are arbitrary byte sequences; on non-Unix platforms we fall
/// back to lossy decoding (the common case still works).
fn path_from_bytes(bytes: &[u8]) -> std::path::PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        std::ffi::OsStr::from_bytes(bytes).into()
    }
    #[cfg(not(unix))]
    {
        std::path::PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

/// Lossy-display a raw path bytes slice for error messages.
fn lossy_path(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Classify a worktree entry at `worktree/rel_path` and read its raw bytes.
///
/// `rel_path` is raw bytes to preserve non-UTF-8 filenames. The resulting
/// [`WorktreeState`] stores the same raw bytes.
///
/// When `is_extra` is `false` (tracked file), a missing entry is recorded as
/// [`TYPE_MISSING`] — a valid deterministic state (the file was deleted from
/// the worktree). When `is_extra` is `true` (untracked/ignored), a missing
/// entry is a traversal-race failure ([`VerificationInputUnavailable`]).
fn classify_worktree_entry(
    worktree: &Path,
    rel_path: &[u8],
    is_extra: bool,
) -> Result<WorktreeState, VerificationInputUnavailable> {
    let full_path = worktree.join(path_from_bytes(rel_path));

    let metadata = match std::fs::symlink_metadata(&full_path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if is_extra {
                return Err(VerificationInputUnavailable::MissingExtraEntry {
                    path: lossy_path(rel_path),
                });
            }
            return Ok(WorktreeState {
                path: rel_path.to_vec(),
                type_tag: TYPE_MISSING,
                mode_tag: MODE_NORMAL,
                content: Vec::new(),
            });
        }
        Err(e) => {
            return Err(VerificationInputUnavailable::UnreadableFile {
                path: lossy_path(rel_path),
                error: e.to_string(),
            });
        }
    };

    let file_type = metadata.file_type();

    if file_type.is_symlink() {
        let target = std::fs::read_link(&full_path).map_err(|e| {
            VerificationInputUnavailable::UnreadableFile {
                path: lossy_path(rel_path),
                error: e.to_string(),
            }
        })?;
        return Ok(WorktreeState {
            path: rel_path.to_vec(),
            type_tag: TYPE_SYMLINK,
            mode_tag: MODE_NORMAL,
            content: symlink_target_bytes(&target),
        });
    }

    if file_type.is_file() {
        let content = std::fs::read(&full_path).map_err(|e| {
            VerificationInputUnavailable::UnreadableFile {
                path: lossy_path(rel_path),
                error: e.to_string(),
            }
        })?;
        return Ok(WorktreeState {
            path: rel_path.to_vec(),
            type_tag: TYPE_REGULAR,
            mode_tag: if is_executable(&metadata) {
                MODE_EXEC
            } else {
                MODE_NORMAL
            },
            content,
        });
    }

    // Remaining types: directory, FIFO, socket, block/char device.
    let kind = entry_kind_label(&file_type);
    Err(VerificationInputUnavailable::UnsupportedSpecialFile {
        path: lossy_path(rel_path),
        kind,
    })
}

/// Collect all untracked and ignored paths (deduplicated, not yet sorted).
///
/// Uses the raw-byte git helper so non-UTF-8 path bytes are preserved exactly.
async fn collect_extra_paths(worktree: &Path) -> Result<Vec<Vec<u8>>, VerificationInputError> {
    let untracked = git_binary_stdout(
        worktree,
        vec![
            "ls-files".into(),
            "--others".into(),
            "--exclude-standard".into(),
            "-z".into(),
        ],
    )
    .await?;
    let ignored = git_binary_stdout(
        worktree,
        vec![
            "ls-files".into(),
            "--others".into(),
            "-i".into(),
            "--exclude-standard".into(),
            "-z".into(),
        ],
    )
    .await?;

    let mut paths: Vec<Vec<u8>> = Vec::new();
    paths.extend(split_nul_paths_bytes(&untracked));
    paths.extend(split_nul_paths_bytes(&ignored));
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn split_nul_paths_bytes(output: &[u8]) -> Vec<Vec<u8>> {
    output
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| p.to_vec())
        .collect()
}

// ─── Internal: platform helpers ─────────────────────────────────────────────

#[cfg(unix)]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &std::fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn symlink_target_bytes(target: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    target.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn symlink_target_bytes(target: &Path) -> Vec<u8> {
    target.to_string_lossy().into_owned().into_bytes()
}

#[cfg(unix)]
fn entry_kind_label(file_type: &std::fs::FileType) -> String {
    use std::os::unix::fs::FileTypeExt;
    if file_type.is_dir() {
        "directory".to_string()
    } else if file_type.is_fifo() {
        "fifo".to_string()
    } else if file_type.is_socket() {
        "socket".to_string()
    } else if file_type.is_block_device() {
        "block_device".to_string()
    } else if file_type.is_char_device() {
        "char_device".to_string()
    } else {
        "special".to_string()
    }
}

#[cfg(not(unix))]
fn entry_kind_label(file_type: &std::fs::FileType) -> String {
    if file_type.is_dir() {
        "directory".to_string()
    } else {
        "special".to_string()
    }
}

// ─── Internal: canonical stream builder ─────────────────────────────────────

/// Length-delimited byte buffer builder for the canonical stream.
///
/// Every variable-length field is written as `[u64 LE length][bytes]`, and
/// every count is written as `[u64 LE]` or `[u32 LE]`. This makes the framing
/// self-delimiting and unambiguous.
struct CanonicalStream {
    buf: Vec<u8>,
}

impl CanonicalStream {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Write a length-delimited field: `[u64 LE len][bytes]`.
    fn field(&mut self, bytes: &[u8]) {
        self.buf
            .extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        self.buf.extend_from_slice(bytes);
    }

    /// Write a raw u32 in little-endian.
    fn u32(&mut self, value: u32) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    /// Write a raw u64 in little-endian.
    fn u64(&mut self, value: u64) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    /// Write the versioned header: magic + version tag + numeric version.
    fn write_header(&mut self) {
        self.field(STREAM_MAGIC);
        self.field(STREAM_VERSION_TAG);
        self.u32(VERIFICATION_INPUT_FINGERPRINT_VERSION_V1);
    }

    /// Write the refs section: merge-base + HEAD.
    fn write_refs(&mut self, merge_base: &str, head: &str) {
        self.field(merge_base.as_bytes());
        self.field(head.as_bytes());
    }

    /// Write the index-entries section.
    ///
    /// Entries **must** be pre-sorted bytewise by path.
    fn write_index_entries(&mut self, entries: &[IndexEntry]) {
        self.u64(entries.len() as u64);
        for entry in entries {
            self.field(&entry.path);
            self.field(&entry.mode);
            self.u32(entry.stage);
            self.field(entry.blob_sha.as_bytes());
        }
    }

    /// Write a worktree-states section (used for both tracked and extra).
    ///
    /// States **must** be pre-sorted bytewise by path.
    fn write_worktree_states(&mut self, states: &[WorktreeState]) {
        self.u64(states.len() as u64);
        for state in states {
            self.field(&state.path);
            self.field(state.type_tag);
            self.field(state.mode_tag);
            self.field(&state.content);
        }
    }

    fn write_external_states(&mut self, states: &[ExternalState]) {
        self.u64(states.len() as u64);
        for external in states {
            self.field(&external.id);
            self.field(&external.locator);
            self.field(&external.path);
            self.field(external.state.type_tag);
            self.field(external.state.mode_tag);
            self.field(&external.state.content);
        }
    }

    fn finalize(self) -> Vec<u8> {
        self.buf
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::test_support::{git, init_repo_with_main_commit, write_and_commit};

    fn write(repo_path: &Path, relative_path: &str, contents: &[u8]) {
        let path = repo_path.join(relative_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent directory");
        }
        std::fs::write(&path, contents).expect("write fixture file");
    }

    fn write_str(repo_path: &Path, relative_path: &str, contents: &str) {
        write(repo_path, relative_path, contents.as_bytes());
    }

    async fn fingerprint(repo_path: &Path) -> VerificationInputFingerprint {
        compute_verification_input_fingerprint(repo_path)
            .await
            .expect("compute fingerprint")
    }

    fn digest(f: VerificationInputFingerprint) -> VerificationInputDigestV1 {
        match f {
            VerificationInputFingerprint::Available(d) => d,
            VerificationInputFingerprint::Unavailable(reason) => {
                panic!("expected available fingerprint, got unavailable: {reason}")
            }
        }
    }

    // ── Basic availability and determinism ──────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clean_repo_produces_available_deterministic_digest() {
        let fixture = init_repo_with_main_commit();

        let first = digest(fingerprint(fixture.path()).await);
        let second = digest(fingerprint(fixture.path()).await);

        assert_eq!(first.version, VERIFICATION_INPUT_FINGERPRINT_VERSION_V1);
        assert_eq!(first.fingerprint.len(), 64);
        assert!(
            first.fingerprint.chars().all(|c| c.is_ascii_hexdigit()),
            "fingerprint should be lowercase hex"
        );
        assert_eq!(first.fingerprint, second.fingerprint);
        assert!(first.merge_base.is_some());
        assert!(!first.head.is_empty());
    }

    // ── Tracked text changes ────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tracked_text_edit_changes_digest() {
        let fixture = init_repo_with_main_commit();
        let before = digest(fingerprint(fixture.path()).await);

        write_str(fixture.path(), "README.md", "hello\nchanged\n");
        let after = digest(fingerprint(fixture.path()).await);

        assert_ne!(
            before.fingerprint, after.fingerprint,
            "dirty tracked edit must change digest"
        );
    }

    // ── Tracked executable mode ─────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tracked_executable_mode_change_alters_digest() {
        let fixture = init_repo_with_main_commit();
        write_str(fixture.path(), "script.sh", "echo hello\n");
        git(fixture.path(), ["add", "script.sh"]);
        git(fixture.path(), ["commit", "-m", "add script"]);

        let before = digest(fingerprint(fixture.path()).await);

        // Toggle the executable bit on the tracked file.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = fixture.path().join("script.sh");
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }

        let after = digest(fingerprint(fixture.path()).await);

        #[cfg(unix)]
        {
            assert_ne!(
                before.fingerprint, after.fingerprint,
                "executable-bit change must alter digest on unix"
            );
        }
        #[cfg(not(unix))]
        {
            let _ = before;
            let _ = after;
        }
    }

    // ── Index-only (staged) changes ─────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn staged_index_change_alters_digest() {
        let fixture = init_repo_with_main_commit();

        // Modify a tracked file but do NOT stage it.
        write_str(fixture.path(), "README.md", "hello\nv2\n");
        let unstaged = digest(fingerprint(fixture.path()).await);

        // Stage the same content — worktree bytes are identical but the index
        // blob SHA changes.
        git(fixture.path(), ["add", "README.md"]);
        let staged = digest(fingerprint(fixture.path()).await);

        assert_ne!(
            unstaged.fingerprint, staged.fingerprint,
            "staging changes the index blob SHA and must alter the digest"
        );
    }

    // ── Ignored generated config ────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ignored_generated_config_alters_digest() {
        let fixture = init_repo_with_main_commit();
        write_str(fixture.path(), ".gitignore", "*.gen\n");
        git(fixture.path(), ["add", ".gitignore"]);
        git(fixture.path(), ["commit", "-m", "ignore generated"]);

        write_str(fixture.path(), "config.gen", "v1\n");
        let before = digest(fingerprint(fixture.path()).await);

        write_str(fixture.path(), "config.gen", "v2\n");
        let after = digest(fingerprint(fixture.path()).await);

        assert_ne!(
            before.fingerprint, after.fingerprint,
            "ignored file content change must alter digest"
        );
    }

    // ── Untracked binary content ────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn untracked_binary_content_alters_digest() {
        let fixture = init_repo_with_main_commit();
        write(fixture.path(), "data.bin", &[0x00, 0x01, 0xFF, 0xFE]);
        let before = digest(fingerprint(fixture.path()).await);

        write(fixture.path(), "data.bin", &[0x00, 0x02, 0xFF, 0xFE]);
        let after = digest(fingerprint(fixture.path()).await);

        assert_ne!(
            before.fingerprint, after.fingerprint,
            "untracked binary content change must alter digest"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nul_and_non_utf8_bytes_are_hashed() {
        let fixture = init_repo_with_main_commit();
        write(fixture.path(), "blob.dat", &[b'a', 0x00, b'b', 0xC3, 0x28]);
        let before = digest(fingerprint(fixture.path()).await);

        // Same length, different bytes.
        write(fixture.path(), "blob.dat", &[b'a', 0x00, b'c', 0xC3, 0x28]);
        let after = digest(fingerprint(fixture.path()).await);

        assert_ne!(before.fingerprint, after.fingerprint);
    }

    // ── Symlink target changes ──────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn symlink_target_change_alters_digest() {
        let fixture = init_repo_with_main_commit();
        write_str(fixture.path(), "target_a.txt", "a\n");
        write_str(fixture.path(), "target_b.txt", "b\n");

        std::os::unix::fs::symlink("target_a.txt", fixture.path().join("link"))
            .expect("create symlink");

        let before = digest(fingerprint(fixture.path()).await);

        std::fs::remove_file(fixture.path().join("link")).unwrap();
        std::os::unix::fs::symlink("target_b.txt", fixture.path().join("link"))
            .expect("recreate symlink");

        let after = digest(fingerprint(fixture.path()).await);

        assert_ne!(
            before.fingerprint, after.fingerprint,
            "symlink target change must alter digest"
        );
    }

    // ── Tracked symlink (index mode 120000) ──────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn tracked_symlink_produces_available_digest_and_alters_on_change() {
        let fixture = init_repo_with_main_commit();
        write_str(fixture.path(), "target_a.txt", "a\n");
        write_str(fixture.path(), "target_b.txt", "b\n");

        // Create and commit a symlink so it is tracked with index mode 120000.
        std::os::unix::fs::symlink("target_a.txt", fixture.path().join("tracked_link"))
            .expect("create symlink");
        git(fixture.path(), ["add", "tracked_link"]);
        git(fixture.path(), ["commit", "-m", "add tracked symlink"]);

        // The tracked symlink must produce an Available digest (not
        // UnsupportedIndexMode).
        let before = match fingerprint(fixture.path()).await {
            VerificationInputFingerprint::Available(d) => d,
            VerificationInputFingerprint::Unavailable(reason) => {
                panic!("tracked symlink should produce Available, got: {reason}")
            }
        };

        // Repoint the tracked symlink target — the worktree symlink state
        // changes and the digest must change too.
        std::fs::remove_file(fixture.path().join("tracked_link")).unwrap();
        std::os::unix::fs::symlink("target_b.txt", fixture.path().join("tracked_link"))
            .expect("recreate tracked symlink");

        let after = digest(fingerprint(fixture.path()).await);

        assert_ne!(
            before.fingerprint, after.fingerprint,
            "tracked symlink target change must alter digest"
        );
    }

    // ── Non-UTF-8 pathname is preserved in the stream ───────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn non_utf8_pathname_is_preserved_and_alters_digest() {
        // A file with a raw 0xFF byte in its name is not valid UTF-8.
        // It must not be silently mangled by lossy conversion.
        let fixture = init_repo_with_main_commit();

        // Create an untracked file with a non-UTF-8 name.
        let non_utf8_name: &[u8] = b"bad\xffname.txt";
        {
            use std::os::unix::ffi::OsStrExt;
            let os_name = std::ffi::OsStr::from_bytes(non_utf8_name);
            let path = fixture.path().join(os_name);
            std::fs::write(&path, b"content\n").expect("write non-utf8 named file");
        }

        let before = digest(fingerprint(fixture.path()).await);

        // Change the content under the same non-UTF-8 name.
        {
            use std::os::unix::ffi::OsStrExt;
            let os_name = std::ffi::OsStr::from_bytes(non_utf8_name);
            let path = fixture.path().join(os_name);
            std::fs::write(&path, b"changed\n").expect("rewrite non-utf8 named file");
        }

        let after = digest(fingerprint(fixture.path()).await);

        assert_ne!(
            before.fingerprint, after.fingerprint,
            "content change under a non-UTF-8 path must alter digest"
        );
    }

    // ── Both untracked and ignored are included ─────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn untracked_and_ignored_are_both_included() {
        let fixture = init_repo_with_main_commit();
        write_str(fixture.path(), ".gitignore", "*.ignored\n");
        git(fixture.path(), ["add", ".gitignore"]);
        git(fixture.path(), ["commit", "-m", "add gitignore"]);

        write_str(fixture.path(), "untracked.txt", "u\n");
        write_str(fixture.path(), "generated.ignored", "i\n");
        let before = digest(fingerprint(fixture.path()).await);

        // Change ignored file only.
        write_str(fixture.path(), "generated.ignored", "i2\n");
        let after_ignored = digest(fingerprint(fixture.path()).await);
        assert_ne!(before.fingerprint, after_ignored.fingerprint);

        // Change untracked file only.
        write_str(fixture.path(), "generated.ignored", "i\n");
        write_str(fixture.path(), "untracked.txt", "u2\n");
        let after_untracked = digest(fingerprint(fixture.path()).await);
        assert_ne!(before.fingerprint, after_untracked.fingerprint);
    }

    // ── Bytewise path ordering ──────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn path_ordering_is_bytewise_and_deterministic() {
        let fixture = init_repo_with_main_commit();

        // Create files in reverse alphabetical order.
        write_str(fixture.path(), "zeta.txt", "z\n");
        write_str(fixture.path(), "alpha.txt", "a\n");
        write_str(fixture.path(), "mid.txt", "m\n");

        let first = digest(fingerprint(fixture.path()).await);

        // Recreate in different order — same content, same paths.
        std::fs::remove_file(fixture.path().join("zeta.txt")).unwrap();
        std::fs::remove_file(fixture.path().join("alpha.txt")).unwrap();
        std::fs::remove_file(fixture.path().join("mid.txt")).unwrap();
        write_str(fixture.path(), "alpha.txt", "a\n");
        write_str(fixture.path(), "mid.txt", "m\n");
        write_str(fixture.path(), "zeta.txt", "z\n");

        let second = digest(fingerprint(fixture.path()).await);

        assert_eq!(
            first.fingerprint, second.fingerprint,
            "creation order must not affect digest — paths are sorted bytewise"
        );
    }

    // ── Special files cause identity unavailable ────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn fifo_at_tracked_path_makes_identity_unavailable() {
        let fixture = init_repo_with_main_commit();

        // Commit a regular file, then replace it with a FIFO in the worktree.
        // git ls-files -s still lists the tracked path, so classify_worktree_entry
        // is invoked and encounters the special file type.
        write_str(fixture.path(), "pipe.txt", "regular\n");
        git(fixture.path(), ["add", "pipe.txt"]);
        git(fixture.path(), ["commit", "-m", "add pipe"]);

        std::fs::remove_file(fixture.path().join("pipe.txt")).unwrap();
        let result = std::process::Command::new("mkfifo")
            .arg(fixture.path().join("pipe.txt"))
            .status()
            .expect("mkfifo");
        assert!(result.success(), "mkfifo should succeed");

        let result = fingerprint(fixture.path()).await;
        assert!(
            result.is_unavailable(),
            "FIFO at tracked path should make identity unavailable, got: {result:?}"
        );
        match result.unavailable_reason().unwrap() {
            VerificationInputUnavailable::UnsupportedSpecialFile { path, kind } => {
                assert_eq!(path, "pipe.txt");
                assert_eq!(kind, "fifo");
            }
            other => panic!("expected UnsupportedSpecialFile, got {other:?}"),
        }
    }

    // ── Configured manifest inputs and outputs ───────────────────────────────

    async fn configured_fingerprint(
        repo_path: &Path,
        config: &VerificationInputFingerprintConfig,
    ) -> VerificationInputFingerprint {
        compute_verification_input_fingerprint_with_config(repo_path, config)
            .await
            .expect("compute configured fingerprint")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn configured_external_content_change_alters_digest() {
        let fixture = init_repo_with_main_commit();
        let external = tempfile::tempdir().expect("create external mount");
        write_str(external.path(), "toolchain/version.txt", "v1\n");

        let mut config = VerificationInputFingerprintConfig::default();
        config.manifest.read_only_external_inputs.push(
            djinn_core::canonical_verify::DeclaredExternalInputV1 {
                id: "toolchain".to_string(),
                locator: "host://toolchain".to_string(),
            },
        );
        config.external_inputs.push(ResolvedExternalInputV1 {
            id: "toolchain".to_string(),
            path: external.path().to_path_buf(),
        });

        let before = digest(configured_fingerprint(fixture.path(), &config).await);
        write_str(external.path(), "toolchain/version.txt", "v2\n");
        let after = digest(configured_fingerprint(fixture.path(), &config).await);

        assert_ne!(before.fingerprint, after.fingerprint);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn configured_output_only_files_are_removed_and_excluded_from_digest() {
        let fixture = init_repo_with_main_commit();
        let mut config = VerificationInputFingerprintConfig::default();
        config.manifest.output_only_globs.push("out/**".to_string());

        write_str(fixture.path(), "out/result.txt", "first generated result\n");
        let first = digest(configured_fingerprint(fixture.path(), &config).await);
        assert!(
            !fixture.path().join("out/result.txt").exists(),
            "configured output-only file must be removed before hashing"
        );

        write_str(
            fixture.path(),
            "out/result.txt",
            "different generated result\n",
        );
        let second = digest(configured_fingerprint(fixture.path(), &config).await);
        assert!(
            !fixture.path().join("out/result.txt").exists(),
            "recreated output-only file must be removed before hashing"
        );
        assert_eq!(first.fingerprint, second.fingerprint);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn configured_overlapping_output_only_globs_fail_before_cleanup() {
        let fixture = init_repo_with_main_commit();
        let output = fixture.path().join("out/result.txt");
        write_str(fixture.path(), "out/result.txt", "must not be deleted\n");

        let mut config = VerificationInputFingerprintConfig::default();
        config
            .manifest
            .output_only_globs
            .extend(["out/**".to_string(), "out/*.txt".to_string()]);
        let result = configured_fingerprint(fixture.path(), &config).await;

        assert!(matches!(
            result,
            VerificationInputFingerprint::Unavailable(
                VerificationInputUnavailable::MalformedManifest { .. }
            )
        ));
        assert!(
            output.exists(),
            "ambiguous declaration must fail before cleanup"
        );
    }

    // ── Unresolved base ref ─────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unresolved_base_ref_makes_identity_unavailable() {
        let fixture = init_repo_with_main_commit();

        let result = compute_verification_input_fingerprint_with_config(
            fixture.path(),
            &VerificationInputFingerprintConfig::new("nonexistent-branch"),
        )
        .await
        .expect("no infra error");

        assert!(result.is_unavailable());
        assert!(matches!(
            result.unavailable_reason(),
            Some(VerificationInputUnavailable::UnresolvedBaseRef { .. })
        ));
    }

    // ── Untracked file vanishing during scan ────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_untracked_entry_is_traversal_race() {
        let fixture = init_repo_with_main_commit();
        write_str(fixture.path(), "ephemeral.txt", "gone soon\n");

        // Manually call the internal classifier after deleting the file to
        // simulate a traversal race.
        std::fs::remove_file(fixture.path().join("ephemeral.txt")).unwrap();
        let result = classify_worktree_entry(fixture.path(), b"ephemeral.txt", true);
        assert!(matches!(
            result,
            Err(VerificationInputUnavailable::MissingExtraEntry { .. })
        ));
    }

    // ── Tracked missing file is valid ───────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn deleted_tracked_file_is_valid_missing_state() {
        let fixture = init_repo_with_main_commit();
        write_str(fixture.path(), "temp.txt", "temp\n");
        git(fixture.path(), ["add", "temp.txt"]);
        git(fixture.path(), ["commit", "-m", "add temp"]);

        // Delete from worktree but leave in index.
        std::fs::remove_file(fixture.path().join("temp.txt")).unwrap();

        let result = fingerprint(fixture.path()).await;
        assert!(
            result.is_available(),
            "deleted tracked file should produce Available with TYPE_MISSING, got: {result:?}"
        );
    }

    // ── Does not call submission_diff ───────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn verification_does_not_depend_on_submission_diff() {
        // This is a structural guarantee: the verification_input module only
        // imports run_git_command_allow_failure / run_git_command_binary_in
        // from the crate root, never compute_submission_diff_fingerprint. We
        // verify the result shape is self-consistent.
        let fixture = init_repo_with_main_commit();
        write_str(fixture.path(), "extra.txt", "x\n");

        let result = fingerprint(fixture.path()).await;
        assert!(result.is_available());
        assert!(result.fingerprint().is_some());
    }

    // ── Golden framing: magic header ────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn canonical_stream_has_stable_magic_header() {
        let fixture = init_repo_with_main_commit();

        // Build the stream via the same code path, capturing raw bytes.
        let head = try_rev_parse(fixture.path(), "HEAD")
            .await
            .unwrap()
            .unwrap();
        let resolved_base = resolve_base_ref(fixture.path(), "main")
            .await
            .unwrap()
            .unwrap();
        let merge_base = try_merge_base(fixture.path(), &resolved_base)
            .await
            .unwrap()
            .unwrap();

        let index_output = git_binary_stdout(
            fixture.path(),
            vec!["ls-files".into(), "-s".into(), "-z".into()],
        )
        .await
        .unwrap();
        let mut index_entries = parse_index_entries(&index_output);
        index_entries.sort_by(|a, b| a.path.cmp(&b.path));

        let mut stream = CanonicalStream::new();
        stream.write_header();
        stream.write_refs(&merge_base, &head);
        stream.write_index_entries(&index_entries);
        stream.write_worktree_states(&[]);
        stream.write_worktree_states(&[]);
        let bytes = stream.finalize();

        // Field 1: magic
        let magic_len = u64::from_le_bytes(bytes[0..8].try_into().unwrap()) as usize;
        assert_eq!(magic_len, STREAM_MAGIC.len());
        assert_eq!(&bytes[8..8 + magic_len], STREAM_MAGIC);

        let offset = 8 + magic_len;

        // Field 2: version tag
        let tag_len = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap()) as usize;
        assert_eq!(tag_len, STREAM_VERSION_TAG.len());
        assert_eq!(&bytes[offset + 8..offset + 8 + tag_len], STREAM_VERSION_TAG);

        let offset = offset + 8 + tag_len;

        // u32 version
        let version = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        assert_eq!(version, VERIFICATION_INPUT_FINGERPRINT_VERSION_V1);
    }

    // ── Multiple distinct changes each alter digest ─────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn new_untracked_file_alters_digest() {
        let fixture = init_repo_with_main_commit();
        let before = digest(fingerprint(fixture.path()).await);

        write_str(fixture.path(), "new.txt", "new\n");
        let after = digest(fingerprint(fixture.path()).await);

        assert_ne!(before.fingerprint, after.fingerprint);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn committed_change_alters_digest() {
        let fixture = init_repo_with_main_commit();
        let before = digest(fingerprint(fixture.path()).await);

        write_and_commit(fixture.path(), "src/new.rs", "pub fn f() {}\n", "add code");
        let after = digest(fingerprint(fixture.path()).await);

        assert_ne!(before.fingerprint, after.fingerprint);
    }
}
