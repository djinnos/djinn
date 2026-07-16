use crate::{CommandOutput, GitError, run_git_command_allow_failure, run_git_command_binary_in};
use djinn_core::canonical_verify::{
    VERIFICATION_INPUT_MANIFEST_VERSION_V1, VerificationInputManifestV1,
};
use globset::{Glob, GlobSet, GlobSetBuilder};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
pub const VERIFICATION_INPUT_FINGERPRINT_VERSION_V1: u32 = 1;
pub const DEFAULT_VERIFICATION_BASE_REF: &str = "main";
const STREAM_MAGIC: &[u8] = b"djinn-verification-input-fingerprint";
const STREAM_VERSION_TAG: &[u8] = b"v1";
const TYPE_REGULAR: &[u8] = b"regular";
const TYPE_SYMLINK: &[u8] = b"symlink";
const TYPE_MISSING: &[u8] = b"missing";
const MODE_EXEC: &[u8] = b"exec";
const MODE_NORMAL: &[u8] = b"normal";
const MODE_GITLINK_TAG: &[u8] = b"160000";
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationInputFingerprintConfig {
    pub base_ref: String,
    pub manifest: VerificationInputManifestV1,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerificationInputFingerprint {
    Available(VerificationInputDigestV1),
    Unavailable(VerificationInputUnavailable),
}
impl VerificationInputFingerprint {
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available(_))
    }
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }
    pub fn fingerprint(&self) -> Option<&str> {
        match self {
            Self::Available(digest) => Some(&digest.fingerprint),
            Self::Unavailable(_) => None,
        }
    }
    pub fn unavailable_reason(&self) -> Option<&VerificationInputUnavailable> {
        match self {
            Self::Available(_) => None,
            Self::Unavailable(reason) => Some(reason),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationInputDigestV1 {
    pub version: u32,
    pub fingerprint: String,
    pub canonical_stream_len: u64,
    pub merge_base: Option<String>,
    pub head: String,
    pub tracked_entry_count: u64,
    pub extra_entry_count: u64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerificationInputUnavailable {
    UnresolvedBaseRef {
        base_ref: String,
    },
    MalformedManifest {
        detail: String,
    },
    MissingExternalInput {
        id: String,
    },
    UnresolvedHead,
    UnsupportedIndexMode {
        path: String,
        mode: String,
    },
    UnsupportedSpecialFile {
        path: String,
        kind: String,
    },
    UnreadableFile {
        path: String,
        error: String,
    },
    MissingExtraEntry {
        path: String,
    },
    UninitializedSubmodule {
        path: String,
    },
    SubmodulePathEscape {
        path: String,
    },
    SubmoduleHeadMismatch {
        path: String,
        expected: String,
        actual: String,
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
            Self::UninitializedSubmodule { path } => {
                write!(
                    f,
                    "verification input unavailable: uninitialized submodule {path}"
                )
            }
            Self::SubmodulePathEscape { path } => {
                write!(
                    f,
                    "verification input unavailable: submodule {path} escaped parent worktree"
                )
            }
            Self::SubmoduleHeadMismatch {
                path,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "verification input unavailable: submodule {path} HEAD {actual} != committed gitlink {expected}"
                )
            }
        }
    }
}
#[derive(Debug, thiserror::Error)]
pub enum VerificationInputError {
    #[error("git command failed: {0}")]
    Git(#[from] GitError),
}
pub async fn compute_verification_input_fingerprint(
    worktree: impl AsRef<Path>,
) -> Result<VerificationInputFingerprint, VerificationInputError> {
    compute_verification_input_fingerprint_with_config(
        worktree,
        &VerificationInputFingerprintConfig::default(),
    )
    .await
}
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
    let head = match try_rev_parse(worktree, "HEAD").await? {
        Some(sha) => sha,
        None => {
            return Ok(VerificationInputFingerprint::Unavailable(
                VerificationInputUnavailable::UnresolvedHead,
            ));
        }
    };
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
    let index_output =
        git_binary_stdout(worktree, vec!["ls-files".into(), "-s".into(), "-z".into()]).await?;
    let mut index_entries = parse_index_entries(&index_output);
    index_entries.retain(|entry| !output_only.is_match(path_from_bytes(&entry.path)));
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
    let mut tracked_states = Vec::with_capacity(index_entries.len());
    let mut gitlink_states: Vec<GitlinkState> = Vec::new();
    for entry in &index_entries {
        if entry.mode == MODE_GITLINK_TAG {
            match collect_gitlink_state(worktree, &entry.path, &entry.blob_sha).await {
                Ok(state) => gitlink_states.push(state),
                Err(unavailable) => {
                    return Ok(VerificationInputFingerprint::Unavailable(unavailable));
                }
            }
        } else {
            match classify_worktree_entry(worktree, &entry.path, false) {
                Ok(state) => tracked_states.push(state),
                Err(unavailable) => {
                    return Ok(VerificationInputFingerprint::Unavailable(unavailable));
                }
            }
        }
    }
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
    index_entries.sort_by(|a, b| a.path.cmp(&b.path));
    tracked_states.sort_by(|a, b| a.path.cmp(&b.path));
    extra_states.sort_by(|a, b| a.path.cmp(&b.path));
    gitlink_states.sort_by(|a, b| a.path.cmp(&b.path));
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
    stream.write_gitlink_states(&gitlink_states);
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
#[derive(Debug, Clone)]
struct IndexEntry {
    path: Vec<u8>,
    mode: Vec<u8>,
    stage: u32,
    blob_sha: String,
}
fn parse_index_entries(output: &[u8]) -> Vec<IndexEntry> {
    let mut entries = Vec::new();
    for record in output.split(|&b| b == 0) {
        if record.is_empty() {
            continue;
        }
        let Some(tab_pos) = record.iter().position(|&b| b == b'\t') else {
            continue;
        };
        let metadata = &record[..tab_pos];
        let path = &record[tab_pos + 1..];
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
fn is_supported_index_mode(mode: &[u8]) -> bool {
    matches!(mode, b"100644" | b"100755" | b"120000" | b"160000")
}
#[derive(Debug, Clone)]
struct WorktreeState {
    path: Vec<u8>,
    type_tag: &'static [u8],
    mode_tag: &'static [u8],
    content: Vec<u8>,
}
#[derive(Debug, Clone)]
struct GitlinkState {
    path: Vec<u8>,
    committed_sha: String,
    submodule_stream: Vec<u8>,
}
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
fn lossy_path(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}
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
    let kind = entry_kind_label(&file_type);
    Err(VerificationInputUnavailable::UnsupportedSpecialFile {
        path: lossy_path(rel_path),
        kind,
    })
}
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
/// Collect a gitlink's canonical state: the committed index SHA frames a
/// recursively computed repository-aware worktree stream for the checked-out
/// submodule. Missing, unreadable, path-escaping, or HEAD-mismatched submodules
/// return identity-unavailable rather than degrading to an ordinary directory
/// walk.
async fn collect_gitlink_state(
    parent_worktree: &Path,
    rel_path: &[u8],
    committed_sha: &str,
) -> Result<GitlinkState, VerificationInputUnavailable> {
    let sub_path = path_from_bytes(rel_path);
    let full_sub_path = parent_worktree.join(&sub_path);
    let canonical_sub_path = std::fs::canonicalize(&full_sub_path).map_err(|_| {
        VerificationInputUnavailable::UninitializedSubmodule {
            path: lossy_path(rel_path),
        }
    })?;
    let canonical_parent = std::fs::canonicalize(parent_worktree).map_err(|_| {
        VerificationInputUnavailable::UninitializedSubmodule {
            path: lossy_path(rel_path),
        }
    })?;
    if !canonical_sub_path.starts_with(&canonical_parent) {
        return Err(VerificationInputUnavailable::SubmodulePathEscape {
            path: lossy_path(rel_path),
        });
    }
    let metadata = std::fs::symlink_metadata(&canonical_sub_path).map_err(|_| {
        VerificationInputUnavailable::UninitializedSubmodule {
            path: lossy_path(rel_path),
        }
    })?;
    if !metadata.file_type().is_dir() {
        return Err(VerificationInputUnavailable::UninitializedSubmodule {
            path: lossy_path(rel_path),
        });
    }
    if !canonical_sub_path.join(".git").exists() {
        return Err(VerificationInputUnavailable::UninitializedSubmodule {
            path: lossy_path(rel_path),
        });
    }
    let sub_head = match try_rev_parse(&canonical_sub_path, "HEAD").await {
        Ok(Some(sha)) => sha,
        Ok(None) => {
            return Err(VerificationInputUnavailable::UninitializedSubmodule {
                path: lossy_path(rel_path),
            });
        }
        Err(e) => {
            return Err(VerificationInputUnavailable::UnreadableFile {
                path: lossy_path(rel_path),
                error: e.to_string(),
            });
        }
    };
    if sub_head != committed_sha {
        return Err(VerificationInputUnavailable::SubmoduleHeadMismatch {
            path: lossy_path(rel_path),
            expected: committed_sha.to_string(),
            actual: sub_head,
        });
    }
    let submodule_stream = collect_submodule_stream(&canonical_sub_path, b"").await?;
    Ok(GitlinkState {
        path: rel_path.to_vec(),
        committed_sha: committed_sha.to_string(),
        submodule_stream,
    })
}
/// Recursively compute a repository-aware canonical stream for a submodule
/// worktree, covering the submodule's own index, tracked/untracked/ignored
/// state, and nested gitlinks. Namespaces entries by a parent-relative prefix
/// so bytewise framing is unambiguous at every recursion level. Returns
/// `Err(unavailable)` when a nested repository is missing, unreadable, or
/// invalid so that the top-level identity becomes unavailable.
fn collect_submodule_stream(
    sub_worktree: &Path,
    namespace: &[u8],
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Vec<u8>, VerificationInputUnavailable>> + Send>,
> {
    Box::pin(collect_submodule_stream_inner(
        sub_worktree.to_path_buf(),
        namespace.to_vec(),
    ))
}
async fn collect_submodule_stream_inner(
    sub_worktree: PathBuf,
    namespace: Vec<u8>,
) -> Result<Vec<u8>, VerificationInputUnavailable> {
    let index_output = git_binary_stdout(
        &sub_worktree,
        vec!["ls-files".into(), "-s".into(), "-z".into()],
    )
    .await
    .map_err(|e| VerificationInputUnavailable::UnreadableFile {
        path: String::from_utf8_lossy(&namespace).into_owned(),
        error: e.to_string(),
    })?;
    let mut index_entries = parse_index_entries(&index_output);
    let mut tracked_states = Vec::with_capacity(index_entries.len());
    let mut gitlink_states: Vec<GitlinkState> = Vec::new();
    for entry in &index_entries {
        if !is_supported_index_mode(&entry.mode) {
            return Err(VerificationInputUnavailable::UnsupportedIndexMode {
                path: lossy_path(&namespace_join(&namespace, &entry.path)),
                mode: lossy_path(&entry.mode),
            });
        }
        if entry.mode == MODE_GITLINK_TAG {
            let namespaced = namespace_join(&namespace, &entry.path);
            match collect_gitlink_state(&sub_worktree, &entry.path, &entry.blob_sha).await {
                Ok(mut state) => {
                    state.path = namespaced;
                    gitlink_states.push(state);
                }
                Err(unavailable) => {
                    return Err(unavailable);
                }
            }
        } else {
            let namespaced = namespace_join(&namespace, &entry.path);
            match classify_worktree_entry(&sub_worktree, &entry.path, false) {
                Ok(mut state) => {
                    state.path = namespaced;
                    tracked_states.push(state);
                }
                Err(unavailable) => {
                    return Err(unavailable);
                }
            }
        }
    }
    let extra_paths = collect_extra_paths(&sub_worktree).await.map_err(|e| {
        VerificationInputUnavailable::UnreadableFile {
            path: String::from_utf8_lossy(&namespace).into_owned(),
            error: e.to_string(),
        }
    })?;
    let mut extra_states = Vec::with_capacity(extra_paths.len());
    for path in &extra_paths {
        let namespaced = namespace_join(&namespace, path);
        match classify_worktree_entry(&sub_worktree, path, true) {
            Ok(mut state) => {
                state.path = namespaced;
                extra_states.push(state);
            }
            Err(unavailable) => {
                return Err(unavailable);
            }
        }
    }
    index_entries.sort_by(|a, b| a.path.cmp(&b.path));
    tracked_states.sort_by(|a, b| a.path.cmp(&b.path));
    extra_states.sort_by(|a, b| a.path.cmp(&b.path));
    gitlink_states.sort_by(|a, b| a.path.cmp(&b.path));
    let mut stream = CanonicalStream::new();
    stream.write_header();
    stream.field(&namespace);
    stream.write_index_entries(&index_entries);
    stream.write_worktree_states(&tracked_states);
    stream.write_gitlink_states(&gitlink_states);
    stream.write_worktree_states(&extra_states);
    Ok(stream.finalize())
}
fn namespace_join(namespace: &[u8], path: &[u8]) -> Vec<u8> {
    if namespace.is_empty() {
        path.to_vec()
    } else {
        let mut joined = Vec::with_capacity(namespace.len() + 1 + path.len());
        joined.extend_from_slice(namespace);
        joined.push(b'/');
        joined.extend_from_slice(path);
        joined
    }
}
fn split_nul_paths_bytes(output: &[u8]) -> Vec<Vec<u8>> {
    output
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| p.to_vec())
        .collect()
}
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
struct CanonicalStream {
    buf: Vec<u8>,
}
impl CanonicalStream {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }
    fn field(&mut self, bytes: &[u8]) {
        self.buf
            .extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        self.buf.extend_from_slice(bytes);
    }
    fn u32(&mut self, value: u32) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }
    fn u64(&mut self, value: u64) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }
    fn write_header(&mut self) {
        self.field(STREAM_MAGIC);
        self.field(STREAM_VERSION_TAG);
        self.u32(VERIFICATION_INPUT_FINGERPRINT_VERSION_V1);
    }
    fn write_refs(&mut self, merge_base: &str, head: &str) {
        self.field(merge_base.as_bytes());
        self.field(head.as_bytes());
    }
    fn write_index_entries(&mut self, entries: &[IndexEntry]) {
        self.u64(entries.len() as u64);
        for entry in entries {
            self.field(&entry.path);
            self.field(&entry.mode);
            self.u32(entry.stage);
            self.field(entry.blob_sha.as_bytes());
        }
    }
    fn write_worktree_states(&mut self, states: &[WorktreeState]) {
        self.u64(states.len() as u64);
        for state in states {
            self.field(&state.path);
            self.field(state.type_tag);
            self.field(state.mode_tag);
            self.field(&state.content);
        }
    }
    fn write_gitlink_states(&mut self, states: &[GitlinkState]) {
        self.u64(states.len() as u64);
        for state in states {
            self.field(&state.path);
            self.field(state.committed_sha.as_bytes());
            self.field(&state.submodule_stream);
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
#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::test_support::{
        TestRepoFixture, configure_local_identity, git, init_repo_with_main_commit,
        write_and_commit,
    };
    use tempfile::TempDir;
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
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tracked_executable_mode_change_alters_digest() {
        let fixture = init_repo_with_main_commit();
        write_str(fixture.path(), "script.sh", "echo hello\n");
        git(fixture.path(), ["add", "script.sh"]);
        git(fixture.path(), ["commit", "-m", "add script"]);
        let before = digest(fingerprint(fixture.path()).await);
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
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn staged_index_change_alters_digest() {
        let fixture = init_repo_with_main_commit();
        write_str(fixture.path(), "README.md", "hello\nv2\n");
        let unstaged = digest(fingerprint(fixture.path()).await);
        git(fixture.path(), ["add", "README.md"]);
        let staged = digest(fingerprint(fixture.path()).await);
        assert_ne!(
            unstaged.fingerprint, staged.fingerprint,
            "staging changes the index blob SHA and must alter the digest"
        );
    }
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
        write(fixture.path(), "blob.dat", &[b'a', 0x00, b'c', 0xC3, 0x28]);
        let after = digest(fingerprint(fixture.path()).await);
        assert_ne!(before.fingerprint, after.fingerprint);
    }
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
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn tracked_symlink_produces_available_digest_and_alters_on_change() {
        let fixture = init_repo_with_main_commit();
        write_str(fixture.path(), "target_a.txt", "a\n");
        write_str(fixture.path(), "target_b.txt", "b\n");
        std::os::unix::fs::symlink("target_a.txt", fixture.path().join("tracked_link"))
            .expect("create symlink");
        git(fixture.path(), ["add", "tracked_link"]);
        git(fixture.path(), ["commit", "-m", "add tracked symlink"]);
        let before = match fingerprint(fixture.path()).await {
            VerificationInputFingerprint::Available(d) => d,
            VerificationInputFingerprint::Unavailable(reason) => {
                panic!("tracked symlink should produce Available, got: {reason}")
            }
        };
        std::fs::remove_file(fixture.path().join("tracked_link")).unwrap();
        std::os::unix::fs::symlink("target_b.txt", fixture.path().join("tracked_link"))
            .expect("recreate tracked symlink");
        let after = digest(fingerprint(fixture.path()).await);
        assert_ne!(
            before.fingerprint, after.fingerprint,
            "tracked symlink target change must alter digest"
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn non_utf8_pathname_is_preserved_and_alters_digest() {
        let fixture = init_repo_with_main_commit();
        let non_utf8_name: &[u8] = b"bad\xffname.txt";
        {
            use std::os::unix::ffi::OsStrExt;
            let os_name = std::ffi::OsStr::from_bytes(non_utf8_name);
            let path = fixture.path().join(os_name);
            std::fs::write(&path, b"content\n").expect("write non-utf8 named file");
        }
        let before = digest(fingerprint(fixture.path()).await);
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
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn untracked_and_ignored_are_both_included() {
        let fixture = init_repo_with_main_commit();
        write_str(fixture.path(), ".gitignore", "*.ignored\n");
        git(fixture.path(), ["add", ".gitignore"]);
        git(fixture.path(), ["commit", "-m", "add gitignore"]);
        write_str(fixture.path(), "untracked.txt", "u\n");
        write_str(fixture.path(), "generated.ignored", "i\n");
        let before = digest(fingerprint(fixture.path()).await);
        write_str(fixture.path(), "generated.ignored", "i2\n");
        let after_ignored = digest(fingerprint(fixture.path()).await);
        assert_ne!(before.fingerprint, after_ignored.fingerprint);
        write_str(fixture.path(), "generated.ignored", "i\n");
        write_str(fixture.path(), "untracked.txt", "u2\n");
        let after_untracked = digest(fingerprint(fixture.path()).await);
        assert_ne!(before.fingerprint, after_untracked.fingerprint);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn path_ordering_is_bytewise_and_deterministic() {
        let fixture = init_repo_with_main_commit();
        write_str(fixture.path(), "zeta.txt", "z\n");
        write_str(fixture.path(), "alpha.txt", "a\n");
        write_str(fixture.path(), "mid.txt", "m\n");
        let first = digest(fingerprint(fixture.path()).await);
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
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn fifo_at_tracked_path_makes_identity_unavailable() {
        let fixture = init_repo_with_main_commit();
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
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_untracked_entry_is_traversal_race() {
        let fixture = init_repo_with_main_commit();
        write_str(fixture.path(), "ephemeral.txt", "gone soon\n");
        std::fs::remove_file(fixture.path().join("ephemeral.txt")).unwrap();
        let result = classify_worktree_entry(fixture.path(), b"ephemeral.txt", true);
        assert!(matches!(
            result,
            Err(VerificationInputUnavailable::MissingExtraEntry { .. })
        ));
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn deleted_tracked_file_is_valid_missing_state() {
        let fixture = init_repo_with_main_commit();
        write_str(fixture.path(), "temp.txt", "temp\n");
        git(fixture.path(), ["add", "temp.txt"]);
        git(fixture.path(), ["commit", "-m", "add temp"]);
        std::fs::remove_file(fixture.path().join("temp.txt")).unwrap();
        let result = fingerprint(fixture.path()).await;
        assert!(
            result.is_available(),
            "deleted tracked file should produce Available with TYPE_MISSING, got: {result:?}"
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn verification_does_not_depend_on_submission_diff() {
        let fixture = init_repo_with_main_commit();
        write_str(fixture.path(), "extra.txt", "x\n");
        let result = fingerprint(fixture.path()).await;
        assert!(result.is_available());
        assert!(result.fingerprint().is_some());
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn canonical_stream_has_stable_magic_header() {
        let fixture = init_repo_with_main_commit();
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
        let magic_len = u64::from_le_bytes(bytes[0..8].try_into().unwrap()) as usize;
        assert_eq!(magic_len, STREAM_MAGIC.len());
        assert_eq!(&bytes[8..8 + magic_len], STREAM_MAGIC);
        let offset = 8 + magic_len;
        let tag_len = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap()) as usize;
        assert_eq!(tag_len, STREAM_VERSION_TAG.len());
        assert_eq!(&bytes[offset + 8..offset + 8 + tag_len], STREAM_VERSION_TAG);
        let offset = offset + 8 + tag_len;
        let version = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        assert_eq!(version, VERIFICATION_INPUT_FINGERPRINT_VERSION_V1);
    }
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

    /// Create a real Git submodule fixture: an outer repo with a checked-out
    /// inner submodule at `sub_path` containing one committed file.
    #[allow(dead_code)]
    struct SubmoduleFixture {
        outer: TestRepoFixture,
        inner: TempDir,
    }

    fn git_with_file_protocol<I, S>(repo_path: &Path, args: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let args: Vec<std::ffi::OsString> = args
            .into_iter()
            .map(|arg| arg.as_ref().to_os_string())
            .collect();
        let output = std::process::Command::new("git")
            .args(["-c", "protocol.file.allow=always"])
            .args(&args)
            .current_dir(repo_path)
            .output()
            .unwrap_or_else(|err| {
                panic!(
                    "failed to run git -c protocol.file.allow=always {args:?} in {}: {err}",
                    repo_path.display()
                )
            });
        assert!(
            output.status.success(),
            "git -c protocol.file.allow=always {:?} failed in {} with status {:?}\nstdout:\n{}\nstderr:\n{}",
            args,
            repo_path.display(),
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn make_submodule_fixture(sub_path: &str) -> SubmoduleFixture {
        let outer = init_repo_with_main_commit();
        let inner = tempfile::tempdir().expect("create inner temp dir");
        git(inner.path(), ["init"]);
        configure_local_identity(inner.path());
        write_str(inner.path(), "README.md", "submodule\n");
        git(inner.path(), ["add", "README.md"]);
        git(inner.path(), ["commit", "-m", "inner init"]);
        git(inner.path(), ["branch", "-m", "main"]);
        git_with_file_protocol(
            outer.path(),
            ["submodule", "add", inner.path().to_str().unwrap(), sub_path],
        );
        git(outer.path(), ["commit", "-m", "add submodule"]);
        SubmoduleFixture { outer, inner }
    }

    fn make_nested_submodule_fixture(outer_sub: &str, inner_sub: &str) -> SubmoduleFixture {
        let outer = init_repo_with_main_commit();
        git(outer.path(), ["config", "protocol.file.allow", "always"]);
        let inner = tempfile::tempdir().expect("create inner temp dir");
        git(inner.path(), ["init"]);
        configure_local_identity(inner.path());
        write_str(inner.path(), "README.md", "inner module\n");
        git(inner.path(), ["add", "README.md"]);
        git(inner.path(), ["commit", "-m", "inner init"]);
        git(inner.path(), ["branch", "-m", "main"]);
        let nested = tempfile::tempdir().expect("create nested temp dir");
        git(nested.path(), ["init"]);
        configure_local_identity(nested.path());
        write_str(nested.path(), "nested.txt", "nested module\n");
        git(nested.path(), ["add", "nested.txt"]);
        git(nested.path(), ["commit", "-m", "nested init"]);
        git(nested.path(), ["branch", "-m", "main"]);
        git_with_file_protocol(
            inner.path(),
            [
                "submodule",
                "add",
                nested.path().to_str().unwrap(),
                inner_sub,
            ],
        );
        git(inner.path(), ["commit", "-m", "add nested submodule"]);
        git_with_file_protocol(
            outer.path(),
            [
                "submodule",
                "add",
                inner.path().to_str().unwrap(),
                outer_sub,
            ],
        );
        git_with_file_protocol(
            outer.path(),
            ["submodule", "update", "--init", "--recursive"],
        );
        git(outer.path(), ["commit", "-m", "add submodule"]);
        SubmoduleFixture { outer, inner }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clean_submodule_produces_stable_available_digest() {
        let fixture = make_submodule_fixture("vendor");
        let first = match fingerprint(fixture.outer.path()).await {
            VerificationInputFingerprint::Available(d) => d,
            VerificationInputFingerprint::Unavailable(reason) => {
                panic!("clean submodule should produce Available, got: {reason}")
            }
        };
        let second = digest(fingerprint(fixture.outer.path()).await);
        assert_eq!(
            first.fingerprint, second.fingerprint,
            "clean submodule repo should be deterministic"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submodule_local_dirtiness_changes_top_level_digest() {
        let fixture = make_submodule_fixture("vendor");
        let before = digest(fingerprint(fixture.outer.path()).await);
        write_str(
            &fixture.outer.path().join("vendor"),
            "dirty.txt",
            "local change\n",
        );
        let after = digest(fingerprint(fixture.outer.path()).await);
        assert_ne!(
            before.fingerprint, after.fingerprint,
            "submodule-local dirtiness must change the top-level digest"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submodule_tracked_file_change_alters_digest() {
        let fixture = make_submodule_fixture("vendor");
        let before = digest(fingerprint(fixture.outer.path()).await);
        write_str(
            &fixture.outer.path().join("vendor"),
            "README.md",
            "changed\n",
        );
        let after = digest(fingerprint(fixture.outer.path()).await);
        assert_ne!(
            before.fingerprint, after.fingerprint,
            "modifying a tracked file inside a submodule must alter the top-level digest"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nested_submodule_dirtiness_changes_top_level_digest() {
        let fixture = make_nested_submodule_fixture("vendor", "nested");
        let before = match fingerprint(fixture.outer.path()).await {
            VerificationInputFingerprint::Available(d) => d,
            VerificationInputFingerprint::Unavailable(reason) => {
                panic!("clean nested submodule should produce Available, got: {reason}")
            }
        };
        let nested_path = fixture.outer.path().join("vendor").join("nested");
        write_str(&nested_path, "dirty.txt", "nested local change\n");
        let after = digest(fingerprint(fixture.outer.path()).await);
        assert_ne!(
            before.fingerprint, after.fingerprint,
            "nested-submodule dirtiness must change the top-level digest"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_submodule_content_fails_closed() {
        let fixture = make_submodule_fixture("vendor");
        let sub_path = fixture.outer.path().join("vendor");
        std::fs::remove_dir_all(&sub_path).expect("remove submodule checkout");
        let result = fingerprint(fixture.outer.path()).await;
        assert!(
            result.is_unavailable(),
            "missing submodule checkout should make identity unavailable, got: {result:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uninitialized_submodule_fails_closed() {
        let fixture = make_submodule_fixture("vendor");
        git(
            fixture.outer.path(),
            ["submodule", "deinit", "-f", "vendor"],
        );
        let result = fingerprint(fixture.outer.path()).await;
        assert!(
            result.is_unavailable(),
            "uninitialized submodule should make identity unavailable, got: {result:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submodule_detached_head_mismatch_fails_closed() {
        let fixture = make_submodule_fixture("vendor");
        let sub_path = fixture.outer.path().join("vendor");
        configure_local_identity(&sub_path);
        write_str(&sub_path, "new_branch_file.txt", "branch\n");
        git(&sub_path, ["checkout", "-b", "other-branch"]);
        git(&sub_path, ["add", "new_branch_file.txt"]);
        git(&sub_path, ["commit", "-m", "other branch commit"]);
        let result = fingerprint(fixture.outer.path()).await;
        assert!(
            result.is_unavailable(),
            "submodule HEAD mismatch should make identity unavailable, got: {result:?}"
        );
    }
}
