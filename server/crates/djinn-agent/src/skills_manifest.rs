//! Generated `skills.json` manifest model + deterministic hashing for the
//! project skill directories.
//!
//! The manifest is **build-generated**, not hand-maintained. Its job is to give
//! `skill_read`/prompt materialization a way to verify that the bytes on disk
//! match what the project shipped — without trusting per-session materialization
//! to be free of tampering or drift.
//!
//! ## Generation
//!
//! [`generate_manifest`] walks `.claude/skills/`, `.opencode/skills/`, and
//! `.djinn/skills/`, loads every skill using the same loader the rest of the
//! agent uses, then computes:
//!
//! * `summary_hash` — sha256 over the disclosure summary
//!   (`name` / `description` / `required`) for G5 progressive disclosure.
//! * `content_hash` — sha256 over the **effective served body**
//!   (`ResolvedSkill.content`), including the sorted `references/` content for
//!   directory `SKILL.md` skills. This is byte-identical to what
//!   [`crate::skills::load_skills`] hands to `skill_read` and prompt
//!   materialization.
//! * `source_files[*].sha256` — per-file sha256 for the top-level skill file
//!   and any `references/` files, with paths in deterministic sorted order.
//!
//! The emitted JSON is byte-stable: source files are sorted by their
//! project-relative path, and `recommended_for_roles` / `tags` lists are
//! preserved in declaration order. Re-running the generator on an unchanged
//! tree produces identical bytes, which is what the CI drift check relies on.
//!
//! ## Drift checking
//!
//! [`check_manifest_drift`] regenerates the complete manifest JSON and compares
//! those bytes against the checked artifact. This is the local/CI guard for
//! stale manifests: run `make skills-manifest-check` to verify, or
//! `make skills-manifest-generate` to update `.djinn/skills.json` after editing
//! a skill or reference file.
//!
//! [`verify_manifest`] re-loads skills and recomputes hashes from disk, then
//! compares them against the manifest. It returns the first mismatch it finds
//! (or `Ok(())` if everything matches). The runtime worker task (T3 in the ihl1
//! wave) wraps this to fail closed on tamper/drift.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::extension_diagnostics::ExtensionDiagnosticFact;
use crate::skills::{
    DEFAULT_TRUST_LEVEL, ResolvedSkill, collect_reference_files, load_skills_with_sources,
    load_skills_with_sources_detailed, manifest_drift_fact, missing_file_fact,
    project_skill_identifier, skill_path,
};

/// Current manifest schema version. Bumped when fields change shape.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Generator identifier embedded in every emitted manifest for traceability.
pub const MANIFEST_GENERATED_BY: &str = "djinn-agent skills manifest generator";

/// Default location of the checked-in manifest, relative to the project root.
/// Kept here so the generator and the future CI check agree without each
/// having to know the other's code.
pub const DEFAULT_MANIFEST_PATH: &str = ".djinn/skills.json";

/// On-disk manifest schema. Serialized to `skills.json` via `serde_json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillsManifest {
    /// Schema version. Bumping forces a regenerate even when bytes match.
    pub schema_version: u32,
    /// Generator identifier, e.g. `"djinn-agent skills manifest generator"`.
    pub generated_by: String,
    /// All skills discovered under the canonical directories, sorted by `id`.
    pub skills: Vec<ManifestSkill>,
}

/// Retain only canonical project-skill identifiers. Native identifiers belong
/// to the platform registry and must not be verified or diagnosed as project
/// extensions.
fn project_requested_names(names: &[String]) -> Vec<String> {
    names
        .iter()
        .filter_map(|name| project_skill_identifier(name))
        .filter(|name| crate::native_skills::native_skill(name).is_none())
        .collect()
}

fn is_frontmatter_failure_already_observed(
    diagnostics: &[ExtensionDiagnosticFact],
    error: &ManifestError,
) -> bool {
    let ManifestError::SkillMissing(skill) = error else {
        return false;
    };
    diagnostics.iter().any(|fact| {
        fact.source_key == *skill
            && fact.phase == djinn_core::extension_diagnostics::ExtensionLoadPhase::Frontmatter
    })
}

fn push_manifest_error_fact(diagnostics: &mut Vec<ExtensionDiagnosticFact>, error: &ManifestError) {
    match error {
        ManifestError::SkillMissing(skill)
        | ManifestError::ReferenceMissing { skill, .. }
        | ManifestError::SourceUnreadable { skill, .. } => {
            push_unique_fact(diagnostics, missing_file_fact(skill));
        }
        ManifestError::SkillNotInManifest(skill)
        | ManifestError::MetadataMismatch { skill, .. }
        | ManifestError::SourceHashMismatch { skill, .. }
        | ManifestError::ContentHashMismatch { skill, .. }
        | ManifestError::SummaryHashMismatch { skill, .. } => {
            push_unique_fact(diagnostics, manifest_drift_fact(skill));
        }
    }
}

fn push_unique_fact(diagnostics: &mut Vec<ExtensionDiagnosticFact>, fact: ExtensionDiagnosticFact) {
    if !diagnostics.iter().any(|existing| {
        existing.source_key == fact.source_key
            && existing.phase == fact.phase
            && existing.remedy_code == fact.remedy_code
    }) {
        diagnostics.push(fact);
    }
}

/// Manifest-aware project-skill load result. The detailed path preserves the
/// legacy fail-closed error separately from safe detector facts so persistence
/// can be integrated without changing current callers.
#[derive(Debug)]
pub(crate) struct DetailedVerifiedSkillsLoad {
    pub skills: Vec<ResolvedSkill>,
    pub diagnostics: Vec<ExtensionDiagnosticFact>,
    pub error: Option<RuntimeSkillManifestError>,
}

/// Regenerate the manifest and compare it byte-for-byte with the checked file.
///
/// This catches every drift class the manifest covers, including added or
/// removed skills that a per-entry verifier cannot see. The returned error is
/// intentionally actionable for contributors and CI logs: it points at the
/// exact generation command instead of only exposing a raw diff.
pub fn check_manifest_drift(
    project_root: &Path,
    manifest_path: &Path,
) -> Result<(), ManifestDriftError> {
    let generated =
        generate_manifest(project_root, None).map_err(|source| ManifestDriftError::Io {
            path: manifest_path.display().to_string(),
            source,
        })?;
    let generated_json = to_pretty_json(&generated)?;
    let checked_json =
        fs::read_to_string(manifest_path).map_err(|source| ManifestDriftError::Io {
            path: manifest_path.display().to_string(),
            source,
        })?;

    if checked_json == generated_json {
        return Ok(());
    }

    Err(ManifestDriftError::Drift {
        manifest_path: manifest_path.display().to_string(),
        update_command: "make skills-manifest-generate".to_string(),
        checked_len: checked_json.len(),
        generated_len: generated_json.len(),
    })
}

/// Errors from the CI/local byte-for-byte drift check.
#[derive(Debug, thiserror::Error)]
pub enum ManifestDriftError {
    /// Could not regenerate or read the checked manifest artifact.
    #[error("failed to read or generate skills manifest `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// Generated manifest serialization failed.
    #[error("failed to serialize regenerated skills manifest: {0}")]
    Serialize(#[from] serde_json::Error),
    /// The checked artifact does not match freshly generated output.
    #[error(
        "skills manifest drift detected at `{manifest_path}`. Run `{update_command}` from the repository root and commit the updated manifest."
    )]
    Drift {
        manifest_path: String,
        update_command: String,
        checked_len: usize,
        generated_len: usize,
    },
}

/// Per-skill manifest entry. Fields mirror the ihl1-roadmap schema sketch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestSkill {
    /// Stable identifier — the requested skill name, also the frontmatter
    /// `name:` (or the request name as fallback). Matches the public id the
    /// agent advertises to the model.
    pub id: String,
    /// Display name from the resolved skill (frontmatter `name:` or fallback).
    pub name: String,
    /// One-line description from the frontmatter, used in the G5 disclosure
    /// summary and exposed to the model.
    pub description: String,
    /// `required:` flag. When `true` the skill is fully inlined under G5
    /// progressive disclosure; otherwise the model must call
    /// `skill_read(name)`.
    pub required: bool,
    /// `trust_level:` frontmatter value, defaulting to `"project"` when
    /// omitted. Free-form string; callers are responsible for interpreting it.
    #[serde(default = "default_trust_level")]
    pub trust_level: String,
    /// `recommended_for_roles:` frontmatter list. Empty when omitted.
    #[serde(default)]
    pub recommended_for_roles: Vec<String>,
    /// `tags:` frontmatter list. Empty when omitted.
    #[serde(default)]
    pub tags: Vec<String>,
    /// `sha256:<hex>` over `name|description|required` — the disclosure
    /// summary the model sees in the system prompt.
    pub summary_hash: String,
    /// `sha256:<hex>` over the effective served body
    /// (`ResolvedSkill.content`), including sorted `references/` content.
    /// Matches what `skill_read` and prompt materialization actually serve.
    pub content_hash: String,
    /// All on-disk files that contribute to this skill, sorted by relative
    /// path. The first entry is the top-level skill file; remaining entries
    /// are reference files in sorted order.
    pub source_files: Vec<ManifestSourceFile>,
}

fn default_trust_level() -> String {
    DEFAULT_TRUST_LEVEL.to_string()
}

/// One source file recorded in the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestSourceFile {
    /// Project-relative path using forward slashes (e.g.
    /// `.djinn/skills/rust-safety.md`). Stable across platforms.
    pub path: String,
    /// Lower-case hex sha256 of the file bytes.
    pub sha256: String,
    /// `skill` for the top-level skill file, `reference` for files under
    /// `references/`.
    pub role: ManifestSourceRole,
}

/// Role tag for a manifest source file entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ManifestSourceRole {
    Skill,
    Reference,
}

/// Errors that can occur while generating or verifying a manifest.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// A skill file referenced in the manifest could not be re-loaded from
    /// disk. Treated as a hard failure during verification.
    #[error("skill `{0}` could not be reloaded from disk")]
    SkillMissing(String),
    /// A loaded skill was not present in the checked manifest. This is a hard
    /// runtime failure when a manifest exists because otherwise new/tampered
    /// materialized skills could be served without a content hash.
    #[error(
        "skill `{0}` is not present in the checked skills manifest; run `make skills-manifest-generate` from the repository root and commit .djinn/skills.json"
    )]
    SkillNotInManifest(String),
    /// A manifest entry's disclosure fields no longer match the loaded skill.
    #[error(
        "skill `{skill}` manifest {field} is `{expected}` but loaded skill has `{actual}`; run `make skills-manifest-generate` if this change is intentional"
    )]
    MetadataMismatch {
        skill: String,
        field: &'static str,
        expected: String,
        actual: String,
    },
    /// A `source_files` entry in the manifest had an unreadable file.
    #[error("source file `{path}` for skill `{skill}` could not be hashed: {source}")]
    SourceUnreadable {
        skill: String,
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// The on-disk file's sha256 did not match the manifest entry.
    #[error(
        "source file `{path}` for skill `{skill}` has hash `{actual}` but manifest records `{expected}`"
    )]
    SourceHashMismatch {
        skill: String,
        path: String,
        expected: String,
        actual: String,
    },
    /// A reference file listed in the manifest could not be re-found.
    #[error("reference file `{path}` for skill `{skill}` is missing from disk")]
    ReferenceMissing { skill: String, path: String },
    /// `content_hash` did not match the recomputed value.
    #[error("skill `{skill}` content_hash is `{actual}` but manifest records `{expected}`")]
    ContentHashMismatch {
        skill: String,
        expected: String,
        actual: String,
    },
    /// `summary_hash` did not match the recomputed value.
    #[error("skill `{skill}` summary_hash is `{actual}` but manifest records `{expected}`")]
    SummaryHashMismatch {
        skill: String,
        expected: String,
        actual: String,
    },
}

/// Runtime errors produced while loading skills under manifest verification.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeSkillManifestError {
    /// The checked manifest exists but could not be read.
    #[error("failed to read checked skills manifest `{path}`: {source}")]
    ManifestRead {
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// The checked manifest exists but is not valid JSON for the schema.
    #[error("failed to parse checked skills manifest `{path}`: {source}")]
    ManifestParse {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    /// A loaded skill did not match the checked manifest.
    #[error("skills manifest verification failed: {0}")]
    Verification(#[from] ManifestError),
}

/// Load the requested skills and, when `.djinn/skills.json` exists, verify the
/// resolved summaries, served bodies, and contributing source files against it.
///
/// Missing manifests are accepted for legacy projects. Existing manifests fail
/// closed: stale/tampered loaded skills are rejected with actionable errors
/// rather than being silently materialized into prompts or returned by
/// `skill_read`.
pub fn load_verified_skills(
    project_root: &Path,
    names: &[String],
) -> Result<Vec<ResolvedSkill>, RuntimeSkillManifestError> {
    let detailed = load_verified_skills_detailed(project_root, names);
    match detailed.error {
        Some(error) => Err(error),
        None => Ok(detailed.skills),
    }
}

/// Load and verify project skills while retaining bounded observations for
/// frontmatter, missing files, and manifest failures. The returned `error`
/// retains `load_verified_skills`' fail-closed behavior when a checked
/// manifest does not verify.
pub(crate) fn load_verified_skills_detailed(
    project_root: &Path,
    names: &[String],
) -> DetailedVerifiedSkillsLoad {
    let requested_names = project_requested_names(names);
    let detailed = load_skills_with_sources_detailed(project_root, &requested_names);
    let mut diagnostics = detailed.diagnostics;
    let loaded = detailed.skills;

    let error = if requested_names.is_empty() {
        None
    } else {
        match read_manifest_if_present(project_root) {
            Ok(Some(manifest)) => {
                verify_loaded_skills(project_root, &manifest, &requested_names, &loaded)
                    .err()
                    .map(|error| {
                        // A manifest-declared top-level file can exist but fail
                        // frontmatter parsing. Its primary loader fact is canonical;
                        // verification still fails closed, but must not add the
                        // contradictory "missing file" observation.
                        if !is_frontmatter_failure_already_observed(&diagnostics, &error) {
                            push_manifest_error_fact(&mut diagnostics, &error);
                        }
                        RuntimeSkillManifestError::Verification(error)
                    })
            }
            Ok(None) => None,
            Err(error) => {
                for requested_name in &requested_names {
                    push_unique_fact(&mut diagnostics, manifest_drift_fact(requested_name));
                }
                Some(error)
            }
        }
    };

    DetailedVerifiedSkillsLoad {
        skills: if error.is_some() {
            Vec::new()
        } else {
            loaded.into_iter().map(|(_, skill, _)| skill).collect()
        },
        diagnostics,
        error,
    }
}

pub(crate) fn load_verified_skills_with_sources(
    project_root: &Path,
    names: &[String],
) -> Result<Vec<(String, ResolvedSkill, PathBuf)>, RuntimeSkillManifestError> {
    let requested_names = project_requested_names(names);
    let loaded = load_skills_with_sources_detailed(project_root, &requested_names).skills;

    if !requested_names.is_empty()
        && let Some(manifest) = read_manifest_if_present(project_root)?
    {
        verify_loaded_skills(project_root, &manifest, &requested_names, &loaded)?;
    }

    Ok(loaded)
}

fn read_manifest_if_present(
    project_root: &Path,
) -> Result<Option<SkillsManifest>, RuntimeSkillManifestError> {
    let manifest_path = project_root.join(DEFAULT_MANIFEST_PATH);
    let json = match fs::read_to_string(&manifest_path) {
        Ok(json) => json,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(RuntimeSkillManifestError::ManifestRead {
                path: manifest_path.display().to_string(),
                source,
            });
        }
    };

    serde_json::from_str(&json).map(Some).map_err(|source| {
        RuntimeSkillManifestError::ManifestParse {
            path: manifest_path.display().to_string(),
            source,
        }
    })
}

fn verify_loaded_skills(
    project_root: &Path,
    manifest: &SkillsManifest,
    requested_names: &[String],
    loaded: &[(String, ResolvedSkill, PathBuf)],
) -> Result<(), ManifestError> {
    for requested_name in requested_names {
        if manifest
            .skills
            .iter()
            .any(|entry| entry.id == *requested_name)
            && !loaded.iter().any(|(id, _, _)| id == requested_name)
        {
            return Err(ManifestError::SkillMissing(requested_name.clone()));
        }
    }

    for (id, skill, source_path) in loaded {
        let entry = manifest
            .skills
            .iter()
            .find(|entry| entry.id == *id)
            .ok_or_else(|| ManifestError::SkillNotInManifest(id.clone()))?;
        verify_manifest_source_files_exist(project_root, entry)?;
        verify_manifest_entry(project_root, entry, skill, source_path)?;
    }

    Ok(())
}

/// Check declared contributing files before comparing derived hashes so a
/// deleted reference is reported as a missing file rather than opaque content
/// drift.
fn verify_manifest_source_files_exist(
    project_root: &Path,
    entry: &ManifestSkill,
) -> Result<(), ManifestError> {
    for source in &entry.source_files {
        let path = project_root.join(source.path.replace('/', std::path::MAIN_SEPARATOR_STR));
        if !path.is_file() {
            return Err(ManifestError::ReferenceMissing {
                skill: entry.id.clone(),
                path: source.path.clone(),
            });
        }
    }
    Ok(())
}

/// Build a manifest for a project by walking its canonical skill directories.
///
/// `project_root` is the repository root (not the worktree — same convention as
/// `load_skills`). When `names` is `None` the generator walks
/// `.claude/skills/`, `.opencode/skills/`, and `.djinn/skills/` for every
/// directory-style and flat-file skill; pass `Some(&[…])` to constrain
/// generation to a fixed list. Empty `names` yields a manifest with zero
/// skills but the correct envelope (schema_version, generated_by, …).
pub fn generate_manifest(
    project_root: &Path,
    names: Option<&[String]>,
) -> std::io::Result<SkillsManifest> {
    let resolved: Vec<(String, ResolvedSkill, PathBuf)> = match names {
        Some(requested) => resolve_named_skills(project_root, requested),
        None => discover_all_skills(project_root)?,
    };

    let mut manifest_skills: Vec<ManifestSkill> = resolved
        .into_iter()
        .map(|(id, skill, source_path)| {
            build_manifest_skill(project_root, &id, &skill, &source_path)
        })
        .collect::<Result<_, _>>()?;

    // Final sort for byte-stability. `id` is the stable identifier.
    manifest_skills.sort_by(|a, b| a.id.cmp(&b.id));

    Ok(SkillsManifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        generated_by: MANIFEST_GENERATED_BY.to_string(),
        skills: manifest_skills,
    })
}

fn build_manifest_skill(
    project_root: &Path,
    id: &str,
    skill: &ResolvedSkill,
    source_path: &Path,
) -> std::io::Result<ManifestSkill> {
    let summary_hash = sha256_prefixed(summary_bytes(skill).as_bytes());
    let content_hash = sha256_prefixed(skill.content.as_bytes());

    let mut source_files: Vec<ManifestSourceFile> = Vec::new();
    let top_level = ManifestSourceFile {
        path: relative_posix_path(project_root, source_path)?,
        sha256: sha256_file(source_path)?,
        role: ManifestSourceRole::Skill,
    };
    source_files.push(top_level);

    if let Some(reference_files) = reference_files_sorted(project_root, source_path) {
        for (relative_path, absolute_path) in reference_files {
            let entry = ManifestSourceFile {
                path: relative_path,
                sha256: sha256_file(&absolute_path)?,
                role: ManifestSourceRole::Reference,
            };
            source_files.push(entry);
        }
    }

    Ok(ManifestSkill {
        id: id.to_string(),
        name: skill.name.clone(),
        description: skill.description.clone(),
        required: skill.required,
        trust_level: skill.trust_level.clone(),
        recommended_for_roles: skill.recommended_for_roles.clone(),
        tags: skill.tags.clone(),
        summary_hash,
        content_hash,
        source_files,
    })
}

/// Recompute hashes from disk and compare to the manifest. Returns the first
/// mismatch (semantic) or `Ok(())` if every entry is intact.
///
/// Used by the CI drift check and the runtime tamper detector in T3. The
/// `project_root` argument is the root the manifest was generated against —
/// callers must pass the same root, otherwise source-file paths won't resolve.
pub fn verify_manifest(
    project_root: &Path,
    manifest: &SkillsManifest,
) -> Result<(), ManifestError> {
    for entry in &manifest.skills {
        // Re-resolve by id (== requested name) so we pick the same file the
        // generator picked, even if the frontmatter `name:` is different.
        let path = skill_path(project_root, &entry.id)
            .ok_or_else(|| ManifestError::SkillMissing(entry.id.clone()))?;
        let resolved = load_skills_with_sources(project_root, std::slice::from_ref(&entry.id))
            .into_iter()
            .next()
            .ok_or_else(|| ManifestError::SkillMissing(entry.id.clone()))?;
        let (skill, _resolved_path) = resolved;
        verify_manifest_entry(project_root, entry, &skill, &path)?;
    }
    Ok(())
}

fn verify_manifest_entry(
    project_root: &Path,
    entry: &ManifestSkill,
    skill: &ResolvedSkill,
    path: &Path,
) -> Result<(), ManifestError> {
    if entry.name != skill.name {
        return Err(ManifestError::MetadataMismatch {
            skill: entry.id.clone(),
            field: "name",
            expected: entry.name.clone(),
            actual: skill.name.clone(),
        });
    }
    if entry.description != skill.description {
        return Err(ManifestError::MetadataMismatch {
            skill: entry.id.clone(),
            field: "description",
            expected: entry.description.clone(),
            actual: skill.description.clone(),
        });
    }
    if entry.required != skill.required {
        return Err(ManifestError::MetadataMismatch {
            skill: entry.id.clone(),
            field: "required",
            expected: entry.required.to_string(),
            actual: skill.required.to_string(),
        });
    }

    let actual_summary = sha256_prefixed(summary_bytes(skill).as_bytes());
    if actual_summary != entry.summary_hash {
        return Err(ManifestError::SummaryHashMismatch {
            skill: entry.id.clone(),
            expected: entry.summary_hash.clone(),
            actual: actual_summary,
        });
    }

    let actual_content = sha256_prefixed(skill.content.as_bytes());
    if actual_content != entry.content_hash {
        return Err(ManifestError::ContentHashMismatch {
            skill: entry.id.clone(),
            expected: entry.content_hash.clone(),
            actual: actual_content,
        });
    }

    // Compare source files. The generator emits them in a stable sorted order,
    // so we can match positionally. Any divergence is a drift signal even if
    // the hashes happen to coincide.
    let expected_paths: Vec<&str> = entry
        .source_files
        .iter()
        .map(|sf| sf.path.as_str())
        .collect();
    let mut actual_paths: Vec<String> =
        vec![relative_posix_path(project_root, path).map_err(|e| {
            ManifestError::SourceUnreadable {
                skill: entry.id.clone(),
                path: entry.id.clone(),
                source: e,
            }
        })?];
    if let Some(refs) = reference_files_sorted(project_root, path) {
        for (relative, _) in refs {
            actual_paths.push(relative);
        }
    }
    if actual_paths != expected_paths {
        return Err(ManifestError::ReferenceMissing {
            skill: entry.id.clone(),
            path: expected_paths
                .iter()
                .find(|p| !actual_paths.iter().any(|a| a == *p))
                .map(|p| p.to_string())
                .unwrap_or_else(|| actual_paths[0].clone()),
        });
    }

    for source in &entry.source_files {
        let absolute = project_root.join(source.path.replace('/', std::path::MAIN_SEPARATOR_STR));
        if !absolute.is_file() {
            return Err(ManifestError::ReferenceMissing {
                skill: entry.id.clone(),
                path: source.path.clone(),
            });
        }
        let actual = sha256_file(&absolute).map_err(|e| ManifestError::SourceUnreadable {
            skill: entry.id.clone(),
            path: source.path.clone(),
            source: e,
        })?;
        if actual != source.sha256 {
            return Err(ManifestError::SourceHashMismatch {
                skill: entry.id.clone(),
                path: source.path.clone(),
                expected: source.sha256.clone(),
                actual,
            });
        }
    }

    Ok(())
}

/// Serialize a manifest to pretty JSON suitable for checking in. Trailing
/// newline matches the rest of the repo's checked-in JSON files.
pub fn to_pretty_json(manifest: &SkillsManifest) -> Result<String, serde_json::Error> {
    let mut json = serde_json::to_string_pretty(manifest)?;
    json.push('\n');
    Ok(json)
}

/// Walk the canonical skill directories and return `(id, ResolvedSkill, path)`
/// for every skill found. Sorted by id for deterministic generation.
fn discover_all_skills(
    project_root: &Path,
) -> std::io::Result<Vec<(String, ResolvedSkill, PathBuf)>> {
    let mut names: Vec<String> = Vec::new();
    for root in [".claude/skills", ".opencode/skills", ".djinn/skills"] {
        collect_skill_names(&project_root.join(root), &mut names)?;
    }
    // Stable iteration order across all sources — `load_skills_with_sources`
    // re-resolves each name against the standard precedence, so duplicates
    // collapse naturally; the manifest keeps one entry per resolved id.
    names.sort();
    names.dedup();

    let mut entries = resolve_named_skills(project_root, &names);
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries)
}

fn resolve_named_skills(
    project_root: &Path,
    requested_names: &[String],
) -> Vec<(String, ResolvedSkill, PathBuf)> {
    requested_names
        .iter()
        .filter_map(|requested_name| {
            let (skill, path) =
                load_skills_with_sources(project_root, std::slice::from_ref(requested_name))
                    .into_iter()
                    .next()?;
            Some((requested_name.clone(), skill, path))
        })
        .collect()
}

fn collect_skill_names(dir: &Path, names: &mut Vec<String>) -> std::io::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if path.is_dir() {
            // Directory skill: `.claude/skills/<name>/SKILL.md` etc.
            if path.join("SKILL.md").is_file() {
                names.push(name);
            } else {
                // Nested directories under `.djinn/skills/` may themselves
                // contain further nested skills (e.g. legacy layouts).
                collect_skill_names(&path, names)?;
            }
        } else if path.is_file()
            && name.ends_with(".md")
            && name != "SKILL.md"
            && let Some(stem) = name.strip_suffix(".md")
        {
            names.push(stem.to_string());
        }
    }
    Ok(())
}

/// Return `(project-relative-posix-path, absolute-path)` for every reference
/// file in the skill's `references/` directory, sorted by relative path. The
/// ordering matches [`crate::skills::skill_references_content`] exactly, so
/// the manifest and the served body agree byte-for-byte.
fn reference_files_sorted(
    project_root: &Path,
    skill_path: &Path,
) -> Option<Vec<(String, PathBuf)>> {
    if skill_path.file_name()? != "SKILL.md" {
        return None;
    }
    let references_dir = skill_path.parent()?.join("references");
    if !references_dir.is_dir() {
        return None;
    }
    let mut files: Vec<(PathBuf, PathBuf)> = Vec::new();
    collect_reference_files(&references_dir, &references_dir, &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Some(
        files
            .into_iter()
            .filter_map(|(relative, absolute)| {
                // `relative` is relative to `references_dir`; join with the
                // references dir absolute path then strip the project root
                // prefix to produce a stable project-relative posix path.
                let full = references_dir.join(&relative);
                let project_relative = full.strip_prefix(project_root).ok()?;
                Some((
                    project_relative.to_string_lossy().replace('\\', "/"),
                    absolute,
                ))
            })
            .collect(),
    )
}

fn summary_bytes(skill: &ResolvedSkill) -> String {
    // Stable, parseable representation. We deliberately avoid JSON here so
    // re-formatting tools can't accidentally re-shape the bytes that go
    // into the digest.
    format!(
        "name={}\ndescription={}\nrequired={}",
        skill.name, skill.description, skill.required
    )
}

fn relative_posix_path(project_root: &Path, path: &Path) -> std::io::Result<String> {
    let relative = path.strip_prefix(project_root).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "path `{}` is not under project root `{}`",
                path.display(),
                project_root.display()
            ),
        )
    })?;
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn sha256_prefixed(bytes: &[u8]) -> String {
    format!("sha256:{}", sha256_hex(bytes))
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let bytes = fs::read(path)?;
    Ok(sha256_hex(&bytes))
}

#[cfg(test)]
#[path = "skills_manifest/tests.rs"]
mod tests;
