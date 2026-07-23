use crate::{CommandOutput, GitError, run_git_command_allow_failure, run_git_command_binary_in};
use djinn_core::canonical_verify::{
    VERIFICATION_INPUT_MANIFEST_VERSION_V1, VerificationInputManifestV1,
};
use globset::{Glob, GlobSet, GlobSetBuilder};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::{Path, PathBuf},
};
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
    #[error("verification changed paths could not resolve base ref {base_ref}")]
    UnresolvedSelectionBaseRef { base_ref: String },
    #[error("verification changed paths have an invalid output-only glob")]
    InvalidSelectionOutputGlob,
}

/// Collect paths for command selection with the fingerprint's base/worktree
/// interpretation, including tracked, untracked, and ignored inputs.
pub async fn collect_verification_changed_paths(
    worktree: impl AsRef<Path>,
    config: &VerificationInputFingerprintConfig,
) -> Result<Vec<String>, VerificationInputError> {
    let worktree = worktree.as_ref();
    let base_ref = match config.base_ref.trim() {
        "" => DEFAULT_VERIFICATION_BASE_REF,
        base_ref => base_ref,
    };
    let resolved_base_ref = resolve_base_ref(worktree, base_ref).await?.ok_or_else(|| {
        VerificationInputError::UnresolvedSelectionBaseRef {
            base_ref: base_ref.to_owned(),
        }
    })?;
    let merge_base = try_merge_base(worktree, &resolved_base_ref)
        .await?
        .ok_or_else(|| VerificationInputError::UnresolvedSelectionBaseRef {
            base_ref: resolved_base_ref,
        })?;
    let tracked = git_binary_stdout(
        worktree,
        vec![
            "diff".into(),
            "--name-only".into(),
            "-z".into(),
            merge_base,
            "--".into(),
        ],
    )
    .await?;
    let mut output_builder = GlobSetBuilder::new();
    for glob in &config.manifest.output_only_globs {
        output_builder
            .add(Glob::new(glob).map_err(|_| VerificationInputError::InvalidSelectionOutputGlob)?);
    }
    let output_only = output_builder
        .build()
        .map_err(|_| VerificationInputError::InvalidSelectionOutputGlob)?;
    let mut paths = std::collections::BTreeSet::new();
    for path in split_nul_paths_bytes(&tracked)
        .into_iter()
        .chain(collect_extra_paths(worktree).await?)
    {
        let path = lossy_path(&path);
        if !output_only.is_match(&path) {
            paths.insert(path);
        }
    }
    Ok(paths.into_iter().collect())
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
    let worktree_anchor = match PermittedRootAnchor::capture(worktree, b".") {
        Ok(anchor) => anchor,
        Err(reason) => return Ok(VerificationInputFingerprint::Unavailable(reason)),
    };
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
            match collect_gitlink_state(&worktree_anchor, &entry.path, &entry.blob_sha).await {
                Ok(state) => gitlink_states.push(state),
                Err(unavailable) => {
                    return Ok(VerificationInputFingerprint::Unavailable(unavailable));
                }
            }
        } else {
            match classify_worktree_entry(&worktree_anchor, &entry.path, false) {
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
        match classify_worktree_entry(&worktree_anchor, path, true) {
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
        let anchor =
            PermittedRootAnchor::capture(&mount.path, declaration.id.as_bytes()).map_err(|_| {
                VerificationInputUnavailable::MissingExternalInput {
                    id: declaration.id.clone(),
                }
            })?;
        collect_external_dir(&anchor, &mount.path, declaration, &mut states)?;
        anchor.validate(declaration.id.as_bytes())?;
    }
    states.sort_by(|a, b| (&a.id, &a.locator, &a.path).cmp(&(&b.id, &b.locator, &b.path)));
    Ok(states)
}
fn collect_external_dir(
    anchor: &PermittedRootAnchor,
    dir: &Path,
    declaration: &djinn_core::canonical_verify::DeclaredExternalInputV1,
    states: &mut Vec<ExternalState>,
) -> Result<(), VerificationInputUnavailable> {
    let dir_rel = dir
        .strip_prefix(&anchor.lexical_root)
        .map_err(|_| unavailable_manifest("external path escaped mount"))?;
    let dir_bytes = path_bytes(dir_rel);
    anchor.canonical_path_within_root(dir, &dir_bytes)?;
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
            .strip_prefix(&anchor.lexical_root)
            .map_err(|_| unavailable_manifest("external path escaped mount"))?;
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|_| unavailable_manifest("external traversal changed"))?;
        if meta.file_type().is_dir() {
            collect_external_dir(anchor, &path, declaration, states)?;
        } else {
            let bytes = path_bytes(rel);
            states.push(ExternalState {
                id: declaration.id.as_bytes().to_vec(),
                locator: declaration.locator.as_bytes().to_vec(),
                path: bytes.clone(),
                state: classify_worktree_entry(anchor, &bytes, false)?,
            });
        }
    }
    anchor.canonical_path_within_root(dir, &dir_bytes)?;
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
#[derive(Debug, Clone, PartialEq, Eq)]
struct EntryIdentity {
    is_file: bool,
    is_symlink: bool,
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    mode: u32,
    #[cfg(unix)]
    ctime: i64,
    #[cfg(unix)]
    ctime_nsec: i64,
}
fn entry_identity(metadata: &std::fs::Metadata) -> EntryIdentity {
    let kind = metadata.file_type();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        EntryIdentity {
            is_file: kind.is_file(),
            is_symlink: kind.is_symlink(),
            len: metadata.len(),
            modified: metadata.modified().ok(),
            dev: metadata.dev(),
            ino: metadata.ino(),
            mode: metadata.mode(),
            ctime: metadata.ctime(),
            ctime_nsec: metadata.ctime_nsec(),
        }
    }
    #[cfg(not(unix))]
    EntryIdentity {
        is_file: kind.is_file(),
        is_symlink: kind.is_symlink(),
        len: metadata.len(),
        modified: metadata.modified().ok(),
    }
}
fn traversal_changed(path: &[u8]) -> VerificationInputUnavailable {
    VerificationInputUnavailable::UnreadableFile {
        path: lossy_path(path),
        error: "entry changed during verification input traversal".into(),
    }
}
#[derive(Debug, Clone)]
struct PermittedRootAnchor {
    lexical_root: PathBuf,
    canonical_root: PathBuf,
    identity: EntryIdentity,
}
impl PermittedRootAnchor {
    fn capture(root: &Path, display_path: &[u8]) -> Result<Self, VerificationInputUnavailable> {
        let metadata = std::fs::symlink_metadata(root).map_err(|e| {
            VerificationInputUnavailable::UnreadableFile {
                path: lossy_path(display_path),
                error: e.to_string(),
            }
        })?;
        if !metadata.file_type().is_dir() {
            return Err(traversal_changed(display_path));
        }
        let canonical_root = std::fs::canonicalize(root).map_err(|e| {
            VerificationInputUnavailable::UnreadableFile {
                path: lossy_path(display_path),
                error: e.to_string(),
            }
        })?;
        Ok(Self {
            lexical_root: root.to_path_buf(),
            canonical_root,
            identity: entry_identity(&metadata),
        })
    }

    fn validate(&self, display_path: &[u8]) -> Result<(), VerificationInputUnavailable> {
        let metadata = std::fs::symlink_metadata(&self.lexical_root)
            .map_err(|_| traversal_changed(display_path))?;
        let canonical = std::fs::canonicalize(&self.lexical_root)
            .map_err(|_| traversal_changed(display_path))?;
        if entry_identity(&metadata) != self.identity || canonical != self.canonical_root {
            return Err(traversal_changed(display_path));
        }
        Ok(())
    }

    fn canonical_path_within_root(
        &self,
        path: &Path,
        rel_path: &[u8],
    ) -> Result<PathBuf, VerificationInputUnavailable> {
        self.validate(rel_path)?;
        let canonical_path = std::fs::canonicalize(path).map_err(|e| {
            VerificationInputUnavailable::UnreadableFile {
                path: lossy_path(rel_path),
                error: e.to_string(),
            }
        })?;
        if !canonical_path.starts_with(&self.canonical_root) {
            return Err(VerificationInputUnavailable::UnreadableFile {
                path: lossy_path(rel_path),
                error: "path escaped permitted root".into(),
            });
        }
        Ok(canonical_path)
    }
}
/// Resolve an entry before consuming it. `symlink_metadata` reports only the
/// final entry, while opening a nested path follows intermediate symlinks.
/// Every object inspected by this traversal must remain below its walk root.
#[cfg(unix)]
fn opened_file_within_root(
    anchor: &PermittedRootAnchor,
    file: &std::fs::File,
    rel_path: &[u8],
) -> Result<(), VerificationInputUnavailable> {
    use std::os::unix::io::AsRawFd;

    let opened_path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
    anchor
        .canonical_path_within_root(&opened_path, rel_path)
        .map(|_| ())
}

#[cfg(not(unix))]
fn opened_file_within_root(
    anchor: &PermittedRootAnchor,
    file: &std::fs::File,
    rel_path: &[u8],
) -> Result<(), VerificationInputUnavailable> {
    // Non-Unix platforms have no portable descriptor-to-path API. The caller
    // re-canonicalizes the pathname after reading as a conservative check.
    let _ = (anchor, file, rel_path);
    Ok(())
}
#[cfg(test)]
type ReadMutationHook = std::sync::Arc<dyn Fn(&Path) + Send + Sync>;
#[cfg(test)]
static READ_MUTATION_HOOK: std::sync::OnceLock<std::sync::Mutex<Option<ReadMutationHook>>> =
    std::sync::OnceLock::new();
#[cfg(test)]
fn set_test_read_mutation_hook(hook: Option<ReadMutationHook>) {
    *READ_MUTATION_HOOK
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .expect("test mutation hook lock") = hook;
}
#[cfg(test)]
fn run_test_read_mutation_hook(path: &Path) {
    let hook = READ_MUTATION_HOOK
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .expect("test mutation hook lock")
        .clone();
    if let Some(hook) = hook {
        hook(path);
    }
}
#[cfg(not(test))]
fn run_test_read_mutation_hook(_path: &Path) {}
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
    anchor: &PermittedRootAnchor,
    rel_path: &[u8],
    is_extra: bool,
) -> Result<WorktreeState, VerificationInputUnavailable> {
    anchor.validate(rel_path)?;
    let full_path = anchor.lexical_root.join(path_from_bytes(rel_path));
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
        let before = entry_identity(&metadata);
        // Link text is an input only while the inspected link remains stable
        // and resolves beneath the traversal's permitted root.
        run_test_read_mutation_hook(&full_path);
        let target = std::fs::read_link(&full_path).map_err(|e| {
            VerificationInputUnavailable::UnreadableFile {
                path: lossy_path(rel_path),
                error: e.to_string(),
            }
        })?;
        anchor.canonical_path_within_root(&full_path, rel_path)?;
        let after =
            std::fs::symlink_metadata(&full_path).map_err(|_| traversal_changed(rel_path))?;
        if entry_identity(&after) != before {
            return Err(traversal_changed(rel_path));
        }
        let target_after =
            std::fs::read_link(&full_path).map_err(|_| traversal_changed(rel_path))?;
        if target_after != target {
            return Err(traversal_changed(rel_path));
        }
        anchor.canonical_path_within_root(&full_path, rel_path)?;
        return Ok(WorktreeState {
            path: rel_path.to_vec(),
            type_tag: TYPE_SYMLINK,
            mode_tag: MODE_NORMAL,
            content: symlink_target_bytes(&target),
        });
    }
    if file_type.is_file() {
        let before = entry_identity(&metadata);
        anchor.canonical_path_within_root(&full_path, rel_path)?;
        run_test_read_mutation_hook(&full_path);
        let mut file = std::fs::File::open(&full_path).map_err(|e| {
            VerificationInputUnavailable::UnreadableFile {
                path: lossy_path(rel_path),
                error: e.to_string(),
            }
        })?;
        let opened_before =
            file.metadata()
                .map_err(|e| VerificationInputUnavailable::UnreadableFile {
                    path: lossy_path(rel_path),
                    error: e.to_string(),
                })?;
        if !opened_before.file_type().is_file() || entry_identity(&opened_before) != before {
            return Err(traversal_changed(rel_path));
        }
        opened_file_within_root(anchor, &file, rel_path)?;
        let mut content = Vec::new();
        file.read_to_end(&mut content).map_err(|e| {
            VerificationInputUnavailable::UnreadableFile {
                path: lossy_path(rel_path),
                error: e.to_string(),
            }
        })?;
        let opened_after =
            file.metadata()
                .map_err(|e| VerificationInputUnavailable::UnreadableFile {
                    path: lossy_path(rel_path),
                    error: e.to_string(),
                })?;
        let path_after =
            std::fs::symlink_metadata(&full_path).map_err(|_| traversal_changed(rel_path))?;
        if entry_identity(&opened_after) != before || entry_identity(&path_after) != before {
            return Err(traversal_changed(rel_path));
        }
        anchor.canonical_path_within_root(&full_path, rel_path)?;
        return Ok(WorktreeState {
            path: rel_path.to_vec(),
            type_tag: TYPE_REGULAR,
            mode_tag: if is_executable(&opened_before) {
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
    parent_anchor: &PermittedRootAnchor,
    rel_path: &[u8],
    committed_sha: &str,
) -> Result<GitlinkState, VerificationInputUnavailable> {
    let sub_path = path_from_bytes(rel_path);
    let full_sub_path = parent_anchor.lexical_root.join(&sub_path);
    let canonical_sub_path = parent_anchor.canonical_path_within_root(&full_sub_path, rel_path)?;
    if !canonical_sub_path.starts_with(&parent_anchor.canonical_root) {
        return Err(VerificationInputUnavailable::SubmodulePathEscape {
            path: lossy_path(rel_path),
        });
    }
    let sub_anchor = PermittedRootAnchor::capture(&full_sub_path, rel_path).map_err(|_| {
        VerificationInputUnavailable::UninitializedSubmodule {
            path: lossy_path(rel_path),
        }
    })?;
    if !sub_anchor.lexical_root.join(".git").exists() {
        return Err(VerificationInputUnavailable::UninitializedSubmodule {
            path: lossy_path(rel_path),
        });
    }
    let sub_head = match try_rev_parse(&sub_anchor.lexical_root, "HEAD").await {
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
    let submodule_stream = collect_submodule_stream(sub_anchor.clone(), b"").await?;
    sub_anchor.validate(rel_path)?;
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
    anchor: PermittedRootAnchor,
    namespace: &[u8],
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Vec<u8>, VerificationInputUnavailable>> + Send>,
> {
    Box::pin(collect_submodule_stream_inner(anchor, namespace.to_vec()))
}
async fn collect_submodule_stream_inner(
    anchor: PermittedRootAnchor,
    namespace: Vec<u8>,
) -> Result<Vec<u8>, VerificationInputUnavailable> {
    anchor.validate(&namespace)?;
    let index_output = git_binary_stdout(
        &anchor.lexical_root,
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
            match collect_gitlink_state(&anchor, &entry.path, &entry.blob_sha).await {
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
            match classify_worktree_entry(&anchor, &entry.path, false) {
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
    let extra_paths = collect_extra_paths(&anchor.lexical_root)
        .await
        .map_err(|e| VerificationInputUnavailable::UnreadableFile {
            path: String::from_utf8_lossy(&namespace).into_owned(),
            error: e.to_string(),
        })?;
    let mut extra_states = Vec::with_capacity(extra_paths.len());
    for path in &extra_paths {
        let namespaced = namespace_join(&namespace, path);
        match classify_worktree_entry(&anchor, path, true) {
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
    anchor.validate(&namespace)?;
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
#[path = "verification_input_tests.rs"]
#[allow(clippy::disallowed_methods)]
mod tests;
