//! Skills loading and injection for agent sessions.
//!
//! Skills are markdown files with YAML frontmatter (`name:`, `description:`) stored
//! in `.djinn/skills/` inside the worktree.  When a role has skills assigned in the
//! DB, this module resolves each name to a file, parses its frontmatter, and builds
//! the "## Available Skills" section appended to the system prompt.
//!
//! Missing skills are logged as warnings and skipped — they never block a session.

use std::path::Path;

/// A skill resolved from disk: name, description (from frontmatter), and full
/// markdown body (everything after the closing `---`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSkill {
    pub name: String,
    pub description: String,
    pub content: String,
}

/// Load and resolve skills from `{worktree_path}/.djinn/skills/`.
///
/// For each name in `skill_names`:
/// - Looks for `{worktree_path}/.djinn/skills/{name}.md`
/// - Parses YAML frontmatter for `name` and `description` fields
/// - On missing file or parse error: logs `warn!` and skips
/// - Returns only successfully resolved skills
///
/// This function is synchronous (blocking file I/O) and should be called
/// from a context where blocking is acceptable (or wrapped with
/// `tokio::task::spawn_blocking` if needed).
pub fn load_skills(worktree_path: &Path, skill_names: &[String]) -> Vec<ResolvedSkill> {
    let skills_dir = worktree_path.join(".djinn").join("skills");
    let mut resolved = Vec::new();

    for name in skill_names {
        let skill_path = skills_dir.join(format!("{name}.md"));
        match std::fs::read_to_string(&skill_path) {
            Ok(content) => match parse_skill_file(name, &content) {
                Some(skill) => {
                    tracing::debug!(
                        skill_name = %name,
                        path = %skill_path.display(),
                        "skills: loaded skill"
                    );
                    resolved.push(skill);
                }
                None => {
                    tracing::warn!(
                        skill_name = %name,
                        path = %skill_path.display(),
                        "skills: skill file missing or malformed frontmatter, skipping"
                    );
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(
                    skill_name = %name,
                    path = %skill_path.display(),
                    "skills: skill '{}' not found, skipping",
                    name
                );
            }
            Err(e) => {
                tracing::warn!(
                    skill_name = %name,
                    path = %skill_path.display(),
                    error = %e,
                    "skills: failed to read skill file, skipping"
                );
            }
        }
    }

    resolved
}

/// Parse a skill markdown file.
///
/// Expected format:
/// ```markdown
/// ---
/// name: rust-safety
/// description: Guidelines for safe Rust code
/// ---
///
/// Full skill content here...
/// ```
///
/// Returns `None` if frontmatter is absent or `description` is missing.
/// The `name` field in frontmatter is used if present; otherwise falls back
/// to the filename-derived `fallback_name`.
fn parse_skill_file(fallback_name: &str, content: &str) -> Option<ResolvedSkill> {
    let content = content.trim_start();

    if !content.starts_with("---") {
        // No frontmatter — treat the entire file as content with no description
        tracing::debug!(
            skill_name = %fallback_name,
            "skills: no frontmatter found in skill file"
        );
        return None;
    }

    // Find the closing `---`
    let after_open = content.get(3..)?.trim_start_matches('\n').trim_start_matches('\r');
    let close_pos = after_open.find("\n---")?;
    let frontmatter = &after_open[..close_pos];
    let body = after_open.get(close_pos + 4..)?.trim_start_matches('\n').trim_start_matches('\r');

    // Parse frontmatter lines for `name:` and `description:`
    let mut fm_name: Option<String> = None;
    let mut fm_description: Option<String> = None;

    for line in frontmatter.lines() {
        if let Some(rest) = line.strip_prefix("name:") {
            fm_name = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("description:") {
            fm_description = Some(rest.trim().to_string());
        }
    }

    let description = fm_description?;
    let name = fm_name.unwrap_or_else(|| fallback_name.to_string());

    Some(ResolvedSkill {
        name,
        description,
        content: body.to_string(),
    })
}

/// Build the "## Available Skills" section to append to the system prompt.
///
/// Format:
/// ```markdown
/// ## Available Skills
///
/// - **rust-safety**: Guidelines for safe Rust code
/// - **git-workflow**: Standard git practices for this project
///
/// ---
///
/// ### Skill: rust-safety
///
/// <full skill content>
///
/// ### Skill: git-workflow
///
/// <full skill content>
/// ```
///
/// Returns an empty string when `skills` is empty.
pub fn format_skills_section(skills: &[ResolvedSkill]) -> String {
    if skills.is_empty() {
        return String::new();
    }

    let mut out = String::from("## Available Skills\n\n");

    // Summary listing
    for skill in skills {
        out.push_str(&format!("- **{}**: {}\n", skill.name, skill.description));
    }

    // Full content for each skill (only if any have non-empty body)
    let has_content = skills.iter().any(|s| !s.content.trim().is_empty());
    if has_content {
        out.push('\n');
        for skill in skills {
            out.push_str(&format!("### Skill: {}\n\n", skill.name));
            if skill.content.trim().is_empty() {
                out.push_str("*(no additional content)*\n");
            } else {
                out.push_str(&skill.content);
                if !skill.content.ends_with('\n') {
                    out.push('\n');
                }
            }
            out.push('\n');
        }
    }

    out
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_skill_dir(tmp: &TempDir) -> std::path::PathBuf {
        let skills_dir = tmp.path().join(".djinn").join("skills");
        fs::create_dir_all(&skills_dir).unwrap();
        skills_dir
    }

    fn write_skill(dir: &std::path::Path, name: &str, content: &str) {
        fs::write(dir.join(format!("{name}.md")), content).unwrap();
    }

    #[test]
    fn parse_skill_with_full_frontmatter() {
        let content = "---\nname: rust-safety\ndescription: Safe Rust guidelines\n---\n\nDo not use unsafe unless necessary.\n";
        let skill = parse_skill_file("rust-safety", content).unwrap();
        assert_eq!(skill.name, "rust-safety");
        assert_eq!(skill.description, "Safe Rust guidelines");
        assert!(skill.content.contains("Do not use unsafe"));
    }

    #[test]
    fn parse_skill_uses_fallback_name_when_no_name_in_frontmatter() {
        let content = "---\ndescription: Git practices\n---\n\nAlways commit with a message.\n";
        let skill = parse_skill_file("git-workflow", content).unwrap();
        assert_eq!(skill.name, "git-workflow");
        assert_eq!(skill.description, "Git practices");
    }

    #[test]
    fn parse_skill_returns_none_when_no_frontmatter() {
        let content = "This skill has no frontmatter.\n";
        let result = parse_skill_file("no-frontmatter", content);
        assert!(result.is_none());
    }

    #[test]
    fn parse_skill_returns_none_when_description_missing() {
        let content = "---\nname: incomplete\n---\n\nNo description field.\n";
        let result = parse_skill_file("incomplete", content);
        assert!(result.is_none());
    }

    #[test]
    fn load_skills_resolves_existing_files() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = make_skill_dir(&tmp);

        write_skill(
            &skills_dir,
            "rust-safety",
            "---\nname: rust-safety\ndescription: Safe Rust\n---\n\nAvoid unsafe.\n",
        );
        write_skill(
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
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = make_skill_dir(&tmp);

        write_skill(
            &skills_dir,
            "exists",
            "---\ndescription: This one exists\n---\n\nContent.\n",
        );

        let names = vec!["exists".to_string(), "missing-skill".to_string()];
        let resolved = load_skills(tmp.path(), &names);

        // Only the existing skill is resolved; missing one does not block
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].description, "This one exists");
    }

    #[test]
    fn load_skills_empty_list_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
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
            },
            ResolvedSkill {
                name: "git-workflow".to_string(),
                description: "Git practices".to_string(),
                content: "Commit small changes.".to_string(),
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
        }];
        let section = format_skills_section(&skills);

        assert!(section.contains("## Available Skills"));
        assert!(section.contains("**my-skill**: My skill description"));
    }
}
