use std::fs;
use std::path::{Path, PathBuf};

use djinn_core::extension_diagnostics::{
    ExtensionLoadPhase, ExtensionLoadRemedyCode, ExtensionLoadSeverity, ExtensionLoadSourceKind,
};

use crate::extension_diagnostics::ExtensionDiagnosticFact;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSkill {
    pub name: String,
    pub description: String,
    pub content: String,
    /// When `true` this skill is always inlined in full into the system prompt,
    /// even under progressive disclosure (G5). Weaker models won't reliably pull
    /// content on demand, so `required: true` skills opt out of disclosure.
    /// Sourced from the skill's `required:` frontmatter key (default `false`).
    pub required: bool,
    /// `trust_level:` frontmatter key, defaulting to `"project"`. Free-form
    /// string so projects can adopt their own taxonomy; the manifest records
    /// whatever the skill declared verbatim.
    pub trust_level: String,
    /// `recommended_for_roles:` frontmatter list. Empty when omitted.
    pub recommended_for_roles: Vec<String>,
    /// `tags:` frontmatter list. Empty when omitted. Each entry is trimmed;
    /// empty/whitespace tags are dropped by the parser.
    pub tags: Vec<String>,
}

/// Project-skill load result that retains bounded observations which the
/// legacy `load_skills` API intentionally skipped. The facts are only for
/// project files resolved from the supplied root; native skills never enter
/// this loader.
#[derive(Debug)]
pub(crate) struct DetailedSkillsLoad {
    pub skills: Vec<ResolvedSkill>,
    #[allow(
        dead_code,
        reason = "session load-attempt correlation consumes project-skill facts in a follow-up integration"
    )]
    pub diagnostics: Vec<ExtensionDiagnosticFact>,
}

#[derive(Debug)]
pub(crate) struct DetailedSkillsWithSourcesLoad {
    pub skills: Vec<(String, ResolvedSkill, PathBuf)>,
    pub diagnostics: Vec<ExtensionDiagnosticFact>,
}

#[derive(Debug)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
    required: bool,
    /// Optional `trust_level:` frontmatter key. Defaults to `"project"` for
    /// skills that do not declare it; legacy skills are unaffected.
    trust_level: String,
    /// Optional `recommended_for_roles:` frontmatter list. Defaults to empty.
    recommended_for_roles: Vec<String>,
    /// Optional `tags:` frontmatter list. Defaults to empty.
    tags: Vec<String>,
}

/// Default `trust_level` used when a skill's frontmatter omits the key. Keeps
/// existing skills manifest-friendly without forcing an edit.
pub(crate) const DEFAULT_TRUST_LEVEL: &str = "project";

/// Returns `true` when the value is a truthy frontmatter flag
/// (`true` / `1` / `yes` / `on`, case-insensitive).
fn parse_bool_flag(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "yes" | "on"
    )
}

/// Parse a YAML-style flow/inline list (e.g. `[worker, reviewer]`) or
/// comma-separated values into a `Vec<String>`. Empty/whitespace entries are
/// dropped; matching `trim_matches('"')` / `'\''` is applied per element.
fn parse_inline_list(value: &str) -> Vec<String> {
    let trimmed = value.trim();
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(trimmed);
    if inner.is_empty() {
        return Vec::new();
    }
    inner
        .split(',')
        .map(|part| {
            part.trim()
                .trim_matches('"')
                .trim_matches('\'')
                .trim()
                .to_string()
        })
        .filter(|part| !part.is_empty())
        .collect()
}

fn parse_frontmatter(frontmatter_raw: &str) -> Option<SkillFrontmatter> {
    let mut frontmatter = SkillFrontmatter {
        name: None,
        description: None,
        required: false,
        trust_level: DEFAULT_TRUST_LEVEL.to_string(),
        recommended_for_roles: Vec::new(),
        tags: Vec::new(),
    };
    for line in frontmatter_raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let Some((key, value)) = line.split_once(':') else {
            continue;
        };

        let value = value
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .to_string();
        match key.trim() {
            "name" => frontmatter.name = Some(value),
            "description" => frontmatter.description = Some(value),
            "required" => frontmatter.required = parse_bool_flag(&value),
            "trust_level" => {
                let trimmed = value.trim();
                if !trimmed.is_empty() {
                    frontmatter.trust_level = trimmed.to_string();
                }
            }
            "recommended_for_roles" => {
                frontmatter.recommended_for_roles = parse_inline_list(&value);
            }
            "tags" => {
                frontmatter.tags = parse_inline_list(&value);
            }
            _ => {}
        }
    }
    Some(frontmatter)
}

pub fn load_skills(project_root: &Path, names: &[String]) -> Vec<ResolvedSkill> {
    load_skills_detailed(project_root, names).skills
}

/// Load project skills while retaining safe facts for skipped files.
///
/// This does not change the compatibility policy of [`load_skills`]: missing
/// and malformed optional project skills remain absent from the returned
/// skill list. Its diagnostics use the requested identifier, never a file
/// path or skill content.
pub(crate) fn load_skills_detailed(project_root: &Path, names: &[String]) -> DetailedSkillsLoad {
    let detailed = load_skills_with_sources_detailed(project_root, names);
    DetailedSkillsLoad {
        skills: detailed
            .skills
            .into_iter()
            .map(|(_, skill, _)| skill)
            .collect(),
        diagnostics: detailed.diagnostics,
    }
}

pub(crate) fn load_skills_with_sources_detailed(
    project_root: &Path,
    names: &[String],
) -> DetailedSkillsWithSourcesLoad {
    let mut skills = Vec::new();
    let mut diagnostics = Vec::new();

    for requested_name in names {
        // A project-skill request is an identifier, not a filesystem path.
        // Reject non-project-relative values before resolution so neither a
        // path escape nor an absolute path can become a diagnostic key.
        let Some(requested_name) = project_skill_identifier(requested_name) else {
            continue;
        };
        // Native skills are loaded from the platform registry later in prompt
        // assembly. They are not project extension material and must never
        // produce project-loader observations.
        if crate::native_skills::native_skill(&requested_name).is_some() {
            continue;
        }

        let Some(path) = skill_path(project_root, &requested_name) else {
            diagnostics.push(missing_file_fact(&requested_name));
            continue;
        };

        match load_skill_detailed(&path, &requested_name) {
            Ok(skill) => skills.push((requested_name, skill, path)),
            Err(LoadSkillFailure::Frontmatter) => {
                diagnostics.push(frontmatter_fact(&requested_name))
            }
            Err(LoadSkillFailure::MissingFile) => {
                diagnostics.push(missing_file_fact(&requested_name))
            }
        }
    }

    DetailedSkillsWithSourcesLoad {
        skills,
        diagnostics,
    }
}

/// Load skills together with the on-disk path each one was resolved from.
///
/// Mirrors [`load_skills`] exactly — same candidate-order resolution, same
/// frontmatter parsing, same `references/` inclusion. Exists so the manifest
/// generator can record per-skill `source_files[].path` without re-resolving
/// names and risking a different file matching between load and manifest.
pub(crate) fn load_skills_with_sources(
    project_root: &Path,
    names: &[String],
) -> Vec<(ResolvedSkill, PathBuf)> {
    load_skills_with_sources_detailed(project_root, names)
        .skills
        .into_iter()
        .map(|(_, skill, path)| (skill, path))
        .collect()
}

#[derive(Debug, Clone, Copy)]
enum LoadSkillFailure {
    Frontmatter,
    MissingFile,
}

fn load_skill_detailed(path: &Path, default_name: &str) -> Result<ResolvedSkill, LoadSkillFailure> {
    let content = fs::read_to_string(path).map_err(|_| LoadSkillFailure::MissingFile)?;
    let references = skill_references_content(path);
    parse_skill_file(default_name, &content, references.as_deref())
        .ok_or(LoadSkillFailure::Frontmatter)
}

pub(crate) fn frontmatter_fact(requested_name: &str) -> ExtensionDiagnosticFact {
    skill_fact(
        requested_name,
        ExtensionLoadPhase::Frontmatter,
        ExtensionLoadRemedyCode::CheckSkillFrontmatter,
        "Project skill frontmatter is malformed or missing required fields.",
    )
}

pub(crate) fn missing_file_fact(requested_name: &str) -> ExtensionDiagnosticFact {
    skill_fact(
        requested_name,
        ExtensionLoadPhase::MissingFile,
        ExtensionLoadRemedyCode::RestoreSkillFile,
        "Requested project skill file is missing.",
    )
}

pub(crate) fn manifest_drift_fact(requested_name: &str) -> ExtensionDiagnosticFact {
    skill_fact(
        requested_name,
        ExtensionLoadPhase::ManifestDrift,
        ExtensionLoadRemedyCode::UpdateSkillManifest,
        "Project skill does not match the checked skill manifest.",
    )
}

fn skill_fact(
    requested_name: &str,
    phase: ExtensionLoadPhase,
    remedy_code: ExtensionLoadRemedyCode,
    summary_material: &str,
) -> ExtensionDiagnosticFact {
    ExtensionDiagnosticFact {
        source_kind: ExtensionLoadSourceKind::ProjectSkill,
        // Callers normally supply a validated identifier. Keep this final
        // boundary defensive because facts may later be produced from a
        // checked manifest whose contents are untrusted project material.
        source_key: project_skill_identifier(requested_name)
            .unwrap_or_else(|| "invalid-project-skill".to_string()),
        phase,
        severity: ExtensionLoadSeverity::Warning,
        remedy_code,
        summary_material: summary_material.to_string(),
    }
}

pub(crate) fn skill_path(project_root: &Path, name: &str) -> Option<PathBuf> {
    let name = project_skill_identifier(name)?;
    let candidates = [
        project_root
            .join(".claude")
            .join("skills")
            .join(&name)
            .join("SKILL.md"),
        project_root
            .join(".opencode")
            .join("skills")
            .join(&name)
            .join("SKILL.md"),
        project_root
            .join(".djinn")
            .join("skills")
            .join(format!("{name}.md")),
        project_root
            .join(".djinn")
            .join("skills")
            .join(&name)
            .join("SKILL.md"),
    ];

    candidates.into_iter().find(|path| path.is_file())
}

/// Return a canonical project-relative skill identifier, rejecting values that
/// could be interpreted as absolute or traversing filesystem paths. Forward
/// slash-separated nested identifiers remain supported for legacy layouts.
pub(crate) fn project_skill_identifier(requested_name: &str) -> Option<String> {
    if requested_name.is_empty() || requested_name.contains('\\') {
        return None;
    }

    let path = Path::new(requested_name);
    if path.is_absolute() {
        return None;
    }

    let mut components = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(component) => components.push(component.to_str()?),
            // `.` is canonicalized away; roots and parents are never valid
            // project skill identifiers.
            std::path::Component::CurDir => {}
            std::path::Component::RootDir | std::path::Component::ParentDir => return None,
            std::path::Component::Prefix(_) => return None,
        }
    }
    (!components.is_empty()).then(|| components.join("/"))
}

fn skill_references_content(skill_path: &Path) -> Option<String> {
    if skill_path.file_name()? != "SKILL.md" {
        return None;
    }

    let references_dir = skill_path.parent()?.join("references");
    if !references_dir.is_dir() {
        return None;
    }

    let mut files = Vec::new();
    collect_reference_files(&references_dir, &references_dir, &mut files);
    if files.is_empty() {
        return None;
    }

    files.sort_by(|a, b| a.0.cmp(&b.0));

    let sections: Vec<String> = files
        .into_iter()
        .filter_map(|(relative_path, path)| {
            let content = fs::read_to_string(path).ok()?;
            let content = content.trim();
            if content.is_empty() {
                return None;
            }

            Some(format!("### {}\n\n{}", relative_path.display(), content))
        })
        .collect();

    if sections.is_empty() {
        None
    } else {
        Some(format!("## References\n\n{}", sections.join("\n\n")))
    }
}

pub(crate) fn collect_reference_files(
    root: &Path,
    current_dir: &Path,
    files: &mut Vec<(PathBuf, PathBuf)>,
) {
    let Ok(entries) = fs::read_dir(current_dir) else {
        return;
    };

    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_reference_files(root, &path, files);
        } else if path.is_file()
            && let Ok(relative_path) = path.strip_prefix(root)
        {
            files.push((relative_path.to_path_buf(), path));
        }
    }
}

fn parse_skill_file(
    default_name: &str,
    content: &str,
    references_content: Option<&str>,
) -> Option<ResolvedSkill> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---\n") {
        return None;
    }

    let rest = &trimmed[4..];
    let end = rest.find("\n---\n")?;
    let frontmatter_raw = &rest[..end];
    let body = &rest[end + 5..];
    let frontmatter = parse_frontmatter(frontmatter_raw)?;
    let required = frontmatter.required;
    let description = frontmatter.description?.trim().to_string();
    if description.is_empty() {
        return None;
    }

    let name = frontmatter
        .name
        .unwrap_or_else(|| default_name.to_string())
        .trim()
        .to_string();
    if name.is_empty() {
        return None;
    }

    let body = body.trim();
    let content = match references_content
        .map(str::trim)
        .filter(|content| !content.is_empty())
    {
        Some(references_content) if !body.is_empty() => {
            format!("{body}\n\n{references_content}")
        }
        Some(references_content) => references_content.to_string(),
        None => body.to_string(),
    };

    Some(ResolvedSkill {
        name,
        description,
        content,
        required,
        trust_level: frontmatter.trust_level,
        recommended_for_roles: frontmatter.recommended_for_roles,
        tags: frontmatter.tags,
    })
}

/// `true` when progressive skill disclosure (G5) is enabled via the
/// `DJINN_PROGRESSIVE_SKILLS` env var. Default OFF — when unset (or not a
/// truthy value) the skills section is byte-identical to the pre-G5 form
/// (every skill fully inlined). When ON, only `required` skills are inlined;
/// non-required skills emit name + description and a note pointing the model
/// at the `skill_read(name)` tool to fetch the body on demand.
pub fn progressive_disclosure_enabled() -> bool {
    std::env::var("DJINN_PROGRESSIVE_SKILLS")
        .map(|v| parse_bool_flag(&v))
        .unwrap_or(false)
}

pub(crate) fn format_skills_section(skills: &[ResolvedSkill]) -> String {
    format_skills_section_with(skills, progressive_disclosure_enabled())
}

/// Render the "## Available Skills" section.
///
/// When `progressive` is `false` the output is byte-identical to the legacy
/// behaviour: every skill's name, description, and full content are inlined.
///
/// When `progressive` is `true`, `required` skills are still inlined in full,
/// but non-required skills emit only their name + description followed by a
/// note directing the model to `skill_read(name)` for the full body.
pub(crate) fn format_skills_section_with(skills: &[ResolvedSkill], progressive: bool) -> String {
    if skills.is_empty() {
        return String::new();
    }

    let mut out = String::from("## Available Skills\n\n");
    for (idx, skill) in skills.iter().enumerate() {
        if idx > 0 {
            out.push_str("\n\n");
        }
        out.push_str(&format!("**{}**: {}", skill.name, skill.description));

        // Inline the full body when disclosure is OFF, or when the skill is
        // marked `required` (weaker models won't reliably pull it on demand).
        let inline_body = !progressive || skill.required;
        if inline_body {
            if !skill.content.trim().is_empty() {
                out.push_str("\n\n");
                out.push_str(skill.content.trim());
            }
        } else if !skill.content.trim().is_empty() {
            out.push_str(&format!(
                "\n\n_Call `skill_read(name=\"{}\")` to load this skill's full content._",
                skill.name
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_skill_dir(tmp: &TempDir, root: &str) -> PathBuf {
        let dir = tmp.path().join(root).join("skills");
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_flat_skill(dir: &Path, name: &str, body: &str) {
        fs::write(dir.join(format!("{name}.md")), body).unwrap();
    }

    fn write_directory_skill(dir: &Path, name: &str, body: &str) {
        let skill_dir = dir.join(name);
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(skill_dir.join("SKILL.md"), body).unwrap();
    }

    #[test]
    fn parse_skill_with_full_frontmatter() {
        let content = "---\nname: rust-safety\ndescription: Safe Rust\n---\n\nAvoid unsafe.\n";
        let skill = parse_skill_file("fallback", content, None).expect("skill should parse");
        assert_eq!(skill.name, "rust-safety");
        assert_eq!(skill.description, "Safe Rust");
        assert_eq!(skill.content, "Avoid unsafe.");
    }

    #[test]
    fn parse_skill_uses_default_name_when_name_missing() {
        let content = "---\ndescription: Git workflow\n---\n\nCommit often.\n";
        let skill = parse_skill_file("git-workflow", content, None).expect("skill should parse");
        assert_eq!(skill.name, "git-workflow");
        assert_eq!(skill.description, "Git workflow");
    }

    #[test]
    fn parse_skill_tolerates_frontmatter_comments_quotes_and_extra_keys() {
        let content = concat!(
            "---\n",
            "# Agent Skills metadata\n",
            "name: \"quoted-name\"\n",
            "\n",
            "description: 'Quoted description'\n",
            "extra: keep-ignoring-this\n",
            "---\n",
            "\n",
            "Body content.\n"
        );

        let skill = parse_skill_file("fallback", content, None).expect("skill should parse");
        assert_eq!(skill.name, "quoted-name");
        assert_eq!(skill.description, "Quoted description");
        assert_eq!(skill.content, "Body content.");
    }

    #[test]
    fn parse_skill_returns_none_when_no_frontmatter() {
        let content = "No frontmatter here.";
        let result = parse_skill_file("no-frontmatter", content, None);
        assert!(result.is_none());
    }

    #[test]
    fn parse_skill_returns_none_when_description_missing() {
        let content = "---\nname: incomplete\n---\n\nNo description field.\n";
        let result = parse_skill_file("incomplete", content, None);
        assert!(result.is_none());
    }

    #[test]
    fn parse_skill_appends_references_to_body() {
        let content = "---\ndescription: Skill\n---\n\nPrimary body.\n";
        let references = "## References\n\n### docs.md\n\nReference content.";

        let skill =
            parse_skill_file("fallback", content, Some(references)).expect("skill should parse");

        assert_eq!(
            skill.content,
            "Primary body.\n\n## References\n\n### docs.md\n\nReference content."
        );
    }

    #[test]
    fn load_skills_resolves_existing_files() {
        let tmp = crate::test_helpers::test_tempdir("djinn-skills-");
        let skills_dir = make_skill_dir(&tmp, ".djinn");

        write_flat_skill(
            &skills_dir,
            "rust-safety",
            "---\nname: rust-safety\ndescription: Safe Rust\n---\n\nAvoid unsafe.\n",
        );
        write_flat_skill(
            &skills_dir,
            "git-workflow",
            "---\ndescription: Git workflow\n---\n\nCommit often.\n",
        );

        let names = vec!["rust-safety".to_string(), "git-workflow".to_string()];
        let resolved = load_skills(tmp.path(), &names);

        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].name, "rust-safety");
        assert_eq!(resolved[0].description, "Safe Rust");
        assert_eq!(resolved[1].name, "git-workflow");
        assert_eq!(resolved[1].description, "Git workflow");
    }

    #[test]
    fn load_skills_missing_skill_is_skipped_not_blocked() {
        let tmp = crate::test_helpers::test_tempdir("djinn-skills-");
        let skills_dir = make_skill_dir(&tmp, ".djinn");

        write_flat_skill(
            &skills_dir,
            "exists",
            "---\ndescription: This one exists\n---\n\nContent.\n",
        );

        let names = vec!["exists".to_string(), "missing-skill".to_string()];
        let resolved = load_skills(tmp.path(), &names);

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].description, "This one exists");
    }

    #[test]
    fn detailed_load_reports_frontmatter_and_missing_files_in_request_order() {
        let tmp = crate::test_helpers::test_tempdir("djinn-skills-detailed-");
        let skills_dir = make_skill_dir(&tmp, ".djinn");
        write_flat_skill(&skills_dir, "broken", "not frontmatter\n");
        write_flat_skill(
            &skills_dir,
            "valid",
            "---\ndescription: Valid project skill\n---\n\nBody.\n",
        );

        let detailed = load_skills_detailed(
            tmp.path(),
            &[
                "missing-first".to_string(),
                "broken".to_string(),
                "valid".to_string(),
                "missing-last".to_string(),
            ],
        );

        assert_eq!(detailed.skills.len(), 1);
        assert_eq!(detailed.skills[0].name, "valid");
        let observed: Vec<_> = detailed
            .diagnostics
            .iter()
            .map(|fact| (fact.source_key.as_str(), fact.phase, fact.remedy_code))
            .collect();
        assert_eq!(
            observed,
            vec![
                (
                    "missing-first",
                    ExtensionLoadPhase::MissingFile,
                    ExtensionLoadRemedyCode::RestoreSkillFile,
                ),
                (
                    "broken",
                    ExtensionLoadPhase::Frontmatter,
                    ExtensionLoadRemedyCode::CheckSkillFrontmatter,
                ),
                (
                    "missing-last",
                    ExtensionLoadPhase::MissingFile,
                    ExtensionLoadRemedyCode::RestoreSkillFile,
                ),
            ]
        );
        assert!(
            detailed
                .diagnostics
                .iter()
                .all(|fact| fact.source_kind == ExtensionLoadSourceKind::ProjectSkill)
        );
        assert!(detailed.diagnostics.iter().all(|fact| {
            !fact.summary_material.contains(tmp.path().to_str().unwrap())
                && !fact.summary_material.contains("not frontmatter")
        }));
    }

    #[test]
    fn detailed_load_rejects_non_project_relative_identifiers_without_facts() {
        let tmp = crate::test_helpers::test_tempdir("djinn-skills-invalid-request-");
        let detailed = load_skills_detailed(
            tmp.path(),
            &[
                "/private/skill".to_string(),
                "../outside-skill".to_string(),
                "..\\outside-skill".to_string(),
            ],
        );

        assert!(detailed.skills.is_empty());
        assert!(detailed.diagnostics.is_empty());
        assert_eq!(
            project_skill_identifier("./nested/skill"),
            Some("nested/skill".to_string())
        );
        assert!(skill_path(tmp.path(), "/private/skill").is_none());
        assert!(skill_path(tmp.path(), "../outside-skill").is_none());
    }

    #[test]
    fn detailed_load_never_diagnoses_native_skills() {
        let tmp = crate::test_helpers::test_tempdir("djinn-skills-native-request-");
        let detailed = load_skills_detailed(tmp.path(), &["visual-spec".to_string()]);

        assert!(detailed.skills.is_empty());
        assert!(detailed.diagnostics.is_empty());
    }

    #[test]
    fn load_skills_prefers_claude_then_opencode_then_djinn() {
        let tmp = crate::test_helpers::test_tempdir("djinn-skills-");
        let claude_skills = make_skill_dir(&tmp, ".claude");
        let opencode_skills = make_skill_dir(&tmp, ".opencode");
        let djinn_skills = make_skill_dir(&tmp, ".djinn");

        write_directory_skill(
            &claude_skills,
            "shared-skill",
            "---\ndescription: Claude version\n---\n\nFrom claude.\n",
        );
        write_directory_skill(
            &opencode_skills,
            "shared-skill",
            "---\ndescription: OpenCode version\n---\n\nFrom opencode.\n",
        );
        write_flat_skill(
            &djinn_skills,
            "shared-skill",
            "---\ndescription: Djinn flat version\n---\n\nFrom djinn flat.\n",
        );

        let resolved = load_skills(tmp.path(), &["shared-skill".to_string()]);

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "shared-skill");
        assert_eq!(resolved[0].description, "Claude version");
        assert_eq!(resolved[0].content, "From claude.");
    }

    #[test]
    fn load_skills_falls_back_from_claude_to_opencode_to_djinn() {
        let tmp = crate::test_helpers::test_tempdir("djinn-skills-");
        let opencode_skills = make_skill_dir(&tmp, ".opencode");
        let djinn_skills = make_skill_dir(&tmp, ".djinn");

        write_directory_skill(
            &opencode_skills,
            "shared-skill",
            "---\ndescription: OpenCode version\n---\n\nFrom opencode.\n",
        );
        write_flat_skill(
            &djinn_skills,
            "shared-skill",
            "---\ndescription: Djinn flat version\n---\n\nFrom djinn flat.\n",
        );

        let resolved = load_skills(tmp.path(), &["shared-skill".to_string()]);

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].description, "OpenCode version");
        assert_eq!(resolved[0].content, "From opencode.");
    }

    #[test]
    fn load_skills_prefers_djinn_flat_file_over_directory() {
        let tmp = crate::test_helpers::test_tempdir("djinn-skills-");
        let djinn_skills = make_skill_dir(&tmp, ".djinn");

        write_flat_skill(
            &djinn_skills,
            "legacy-skill",
            "---\ndescription: Flat file version\n---\n\nFrom flat file.\n",
        );
        write_directory_skill(
            &djinn_skills,
            "legacy-skill",
            "---\ndescription: Directory version\n---\n\nFrom directory.\n",
        );

        let resolved = load_skills(tmp.path(), &["legacy-skill".to_string()]);

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].description, "Flat file version");
        assert_eq!(resolved[0].content, "From flat file.");
    }

    #[test]
    fn load_skills_uses_requested_name_when_frontmatter_name_missing_in_directory_skill() {
        let tmp = crate::test_helpers::test_tempdir("djinn-skills-");
        let claude_skills = make_skill_dir(&tmp, ".claude");

        write_directory_skill(
            &claude_skills,
            "fallback-name",
            "---\ndescription: Uses fallback name\n---\n\nContent.\n",
        );

        let resolved = load_skills(tmp.path(), &["fallback-name".to_string()]);

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "fallback-name");
        assert_eq!(resolved[0].description, "Uses fallback name");
    }

    #[test]
    fn load_skills_appends_directory_references_in_sorted_order() {
        let tmp = crate::test_helpers::test_tempdir("djinn-skills-");
        let claude_skills = make_skill_dir(&tmp, ".claude");
        let skill_dir = claude_skills.join("ref-skill");
        let references_dir = skill_dir.join("references");

        fs::create_dir_all(references_dir.join("nested")).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: Skill with references\n---\n\nPrimary body.\n",
        )
        .unwrap();
        fs::write(references_dir.join("z-last.md"), "Zed reference\n").unwrap();
        fs::write(references_dir.join("a-first.md"), "Alpha reference\n").unwrap();
        fs::write(references_dir.join("empty.md"), "   \n\n").unwrap();
        fs::write(
            references_dir.join("nested").join("b-middle.md"),
            "Nested reference\n",
        )
        .unwrap();

        let resolved = load_skills(tmp.path(), &["ref-skill".to_string()]);

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].description, "Skill with references");
        assert_eq!(
            resolved[0].content,
            concat!(
                "Primary body.\n\n",
                "## References\n\n",
                "### a-first.md\n\n",
                "Alpha reference\n\n",
                "### nested/b-middle.md\n\n",
                "Nested reference\n\n",
                "### z-last.md\n\n",
                "Zed reference"
            )
        );
    }

    #[test]
    fn load_skills_flat_files_do_not_include_directory_references() {
        let tmp = crate::test_helpers::test_tempdir("djinn-skills-");
        let djinn_skills = make_skill_dir(&tmp, ".djinn");
        let ignored_references = djinn_skills.join("flat-skill").join("references");

        write_flat_skill(
            &djinn_skills,
            "flat-skill",
            "---\ndescription: Flat skill\n---\n\nFlat body.\n",
        );
        fs::create_dir_all(&ignored_references).unwrap();
        fs::write(ignored_references.join("ignored.md"), "Ignored reference\n").unwrap();

        let resolved = load_skills(tmp.path(), &["flat-skill".to_string()]);

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].content, "Flat body.");
    }

    #[test]
    fn load_skills_empty_list_returns_empty() {
        let tmp = crate::test_helpers::test_tempdir("djinn-skills-");
        let resolved = load_skills(tmp.path(), &[]);
        assert!(resolved.is_empty());
    }

    #[test]
    fn format_skills_section_empty_returns_empty_string() {
        let section = format_skills_section(&[]);
        assert!(section.is_empty());
    }

    #[test]
    fn format_skills_section_includes_name_and_description() {
        let skills = vec![
            ResolvedSkill {
                name: "rust-safety".to_string(),
                description: "Safe Rust guidelines".to_string(),
                content: "Avoid unsafe blocks.".to_string(),
                required: false,
                ..sample_metadata()
            },
            ResolvedSkill {
                name: "git-workflow".to_string(),
                description: "Git practices".to_string(),
                content: "Commit small changes.".to_string(),
                required: false,
                ..sample_metadata()
            },
        ];
        let section = format_skills_section(&skills);

        assert!(section.contains("## Available Skills"));
        assert!(section.contains("**rust-safety**: Safe Rust guidelines"));
        assert!(section.contains("**git-workflow**: Git practices"));
        assert!(section.contains("Avoid unsafe blocks."));
        assert!(section.contains("Commit small changes."));
    }

    #[test]
    fn format_skills_section_single_skill_no_extra_delimiter() {
        let skills = vec![ResolvedSkill {
            name: "my-skill".to_string(),
            description: "My skill description".to_string(),
            content: String::new(),
            required: false,
            ..sample_metadata()
        }];
        let section = format_skills_section(&skills);

        assert!(section.contains("## Available Skills"));
        assert!(section.contains("**my-skill**: My skill description"));
    }

    fn sample_metadata() -> ResolvedSkill {
        ResolvedSkill {
            name: String::new(),
            description: String::new(),
            content: String::new(),
            required: false,
            trust_level: String::new(),
            recommended_for_roles: Vec::new(),
            tags: Vec::new(),
        }
    }

    fn sample_skills() -> Vec<ResolvedSkill> {
        vec![
            ResolvedSkill {
                name: "rust-safety".to_string(),
                description: "Safe Rust guidelines".to_string(),
                content: "Avoid unsafe blocks.".to_string(),
                required: false,
                ..sample_metadata()
            },
            ResolvedSkill {
                name: "house-rules".to_string(),
                description: "Mandatory house rules".to_string(),
                content: "Always run the tests.".to_string(),
                required: true,
                ..sample_metadata()
            },
        ]
    }

    #[test]
    fn parse_skill_required_flag_defaults_false() {
        let content = "---\ndescription: Skill\n---\n\nBody.\n";
        let skill = parse_skill_file("fallback", content, None).expect("skill should parse");
        assert!(!skill.required);
    }

    #[test]
    fn parse_skill_required_flag_truthy_values() {
        for raw in ["true", "1", "yes", "on", "TRUE", "Yes"] {
            let content = format!("---\ndescription: Skill\nrequired: {raw}\n---\n\nBody.\n");
            let skill = parse_skill_file("fallback", &content, None).expect("skill should parse");
            assert!(skill.required, "expected `{raw}` to be truthy");
        }
        // Non-truthy stays false.
        let content = "---\ndescription: Skill\nrequired: false\n---\n\nBody.\n";
        let skill = parse_skill_file("fallback", content, None).expect("skill should parse");
        assert!(!skill.required);
    }

    #[test]
    fn format_skills_section_off_inlines_all_content_byte_identical() {
        // With disclosure OFF, the rendered section must match the legacy form
        // exactly: every skill's full body inlined regardless of `required`.
        let skills = sample_skills();
        let section = format_skills_section_with(&skills, false);
        let expected = concat!(
            "## Available Skills\n\n",
            "**rust-safety**: Safe Rust guidelines\n\n",
            "Avoid unsafe blocks.\n\n",
            "**house-rules**: Mandatory house rules\n\n",
            "Always run the tests."
        );
        assert_eq!(section, expected);
    }

    #[test]
    fn parse_skill_metadata_defaults_when_keys_omitted() {
        // Skills without the new optional keys must still parse and expose
        // safe defaults — preserving legacy behaviour.
        let content = "---\ndescription: Plain skill\n---\n\nBody.\n";
        let skill = parse_skill_file("plain", content, None).expect("skill should parse");
        assert_eq!(skill.trust_level, "project");
        assert!(skill.recommended_for_roles.is_empty());
        assert!(skill.tags.is_empty());
        assert!(!skill.required);
    }

    #[test]
    fn parse_skill_metadata_reads_trust_level_roles_and_tags() {
        let content = "---\nname: annotated\ndescription: With metadata\ntrust_level: trusted\nrecommended_for_roles: [worker, reviewer]\ntags: [alpha, beta]\n---\n\nBody.\n";
        let skill = parse_skill_file("fallback", content, None).expect("skill should parse");
        assert_eq!(skill.trust_level, "trusted");
        assert_eq!(skill.recommended_for_roles, vec!["worker", "reviewer"]);
        assert_eq!(skill.tags, vec!["alpha", "beta"]);
    }

    #[test]
    fn parse_skill_metadata_supports_comma_separated_lists() {
        // Comma-separated values (no brackets) should also parse, since the
        // existing frontmatter style is plain `key: a, b, c`.
        let content = "---\ndescription: Comma\nrecommended_for_roles: worker, reviewer\ntags: alpha, beta, gamma\n---\n\nBody.\n";
        let skill = parse_skill_file("fallback", content, None).expect("skill should parse");
        assert_eq!(skill.recommended_for_roles, vec!["worker", "reviewer"]);
        assert_eq!(skill.tags, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn parse_skill_metadata_drops_empty_list_entries() {
        let content =
            "---\ndescription: Empty tags\ntags: [\"\", alpha, \"  \", beta]\n---\n\nBody.\n";
        let skill = parse_skill_file("fallback", content, None).expect("skill should parse");
        assert_eq!(skill.tags, vec!["alpha", "beta"]);
    }

    #[test]
    fn parse_skill_empty_trust_level_falls_back_to_default() {
        // An explicit-but-empty `trust_level:` should fall back to the default
        // so the manifest never records an empty value.
        let content = "---\ndescription: Empty trust\ntrust_level: \"\"\n---\n\nBody.\n";
        let skill = parse_skill_file("fallback", content, None).expect("skill should parse");
        assert_eq!(skill.trust_level, "project");
    }

    #[test]
    fn load_skills_with_sources_returns_source_path_per_skill() {
        let tmp = crate::test_helpers::test_tempdir("djinn-skills-sources-");
        let skills_dir = make_skill_dir(&tmp, ".djinn");
        write_flat_skill(
            &skills_dir,
            "with-path",
            "---\ndescription: Has path\n---\n\nBody.\n",
        );

        let resolved = load_skills_with_sources(tmp.path(), &["with-path".to_string()]);
        assert_eq!(resolved.len(), 1);
        let (skill, path) = &resolved[0];
        assert_eq!(skill.name, "with-path");
        assert!(
            path.ends_with("with-path.md"),
            "expected source path to end with with-path.md, got {:?}",
            path
        );
    }

    #[test]
    fn format_skills_section_on_discloses_only_nonrequired() {
        let skills = sample_skills();
        let section = format_skills_section_with(&skills, true);

        // Non-required skill: name + description + skill_read note, body NOT inlined.
        assert!(section.contains("**rust-safety**: Safe Rust guidelines"));
        assert!(section.contains("skill_read(name=\"rust-safety\")"));
        assert!(
            !section.contains("Avoid unsafe blocks."),
            "non-required body must not be inlined under disclosure"
        );

        // Required skill: fully inlined, no skill_read note.
        assert!(section.contains("**house-rules**: Mandatory house rules"));
        assert!(section.contains("Always run the tests."));
        assert!(!section.contains("skill_read(name=\"house-rules\")"));
    }

    #[test]
    fn progressive_disclosure_default_is_off_without_env() {
        // The default helper reads the env var; without it set the section
        // renders the inlined (legacy) form. We assert the *default* path here
        // rather than mutating process env (which would race other tests).
        if std::env::var_os("DJINN_PROGRESSIVE_SKILLS").is_some() {
            // Skip when the env var is externally set — this test asserts the
            // unset-default behaviour and must not race that configuration.
            return;
        }
        assert!(!progressive_disclosure_enabled());
        let skills = sample_skills();
        assert_eq!(
            format_skills_section_with(&skills, false),
            format_skills_section(&skills),
            "default format_skills_section must equal the OFF form when env is unset"
        );
    }
}
