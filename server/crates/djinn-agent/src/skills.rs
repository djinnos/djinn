use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSkill {
    pub name: String,
    pub description: String,
    pub content: String,
}

#[derive(Debug)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
}

fn parse_frontmatter(frontmatter_raw: &str) -> Option<SkillFrontmatter> {
    let mut frontmatter = SkillFrontmatter {
        name: None,
        description: None,
    };
    for line in frontmatter_raw.lines() {
        let (key, value) = line.split_once(':')?;
        let value = value.trim().trim_matches('"').to_string();
        match key.trim() {
            "name" => frontmatter.name = Some(value),
            "description" => frontmatter.description = Some(value),
            _ => {}
        }
    }
    Some(frontmatter)
}

pub fn load_skills(project_root: &Path, names: &[String]) -> Vec<ResolvedSkill> {
    names
        .iter()
        .filter_map(|name| {
            let path = skill_path(project_root, name)?;
            let content = fs::read_to_string(path).ok()?;
            parse_skill_file(name, &content)
        })
        .collect()
}

fn skill_path(project_root: &Path, name: &str) -> Option<PathBuf> {
    let candidates = [
        project_root
            .join(".claude")
            .join("skills")
            .join(name)
            .join("SKILL.md"),
        project_root
            .join(".opencode")
            .join("skills")
            .join(name)
            .join("SKILL.md"),
        project_root
            .join(".djinn")
            .join("skills")
            .join(format!("{name}.md")),
        project_root
            .join(".djinn")
            .join("skills")
            .join(name)
            .join("SKILL.md"),
    ];

    candidates.into_iter().find(|path| path.is_file())
}

fn parse_skill_file(default_name: &str, content: &str) -> Option<ResolvedSkill> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---\n") {
        return None;
    }

    let rest = &trimmed[4..];
    let end = rest.find("\n---\n")?;
    let frontmatter_raw = &rest[..end];
    let body = &rest[end + 5..];
    let frontmatter = parse_frontmatter(frontmatter_raw)?;
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

    Some(ResolvedSkill {
        name,
        description,
        content: body.trim().to_string(),
    })
}

pub(crate) fn format_skills_section(skills: &[ResolvedSkill]) -> String {
    if skills.is_empty() {
        return String::new();
    }

    let mut out = String::from("## Available Skills\n\n");
    for (idx, skill) in skills.iter().enumerate() {
        if idx > 0 {
            out.push_str("\n\n");
        }
        out.push_str(&format!("**{}**: {}", skill.name, skill.description));
        if !skill.content.trim().is_empty() {
            out.push_str("\n\n");
            out.push_str(skill.content.trim());
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
        let skill = parse_skill_file("fallback", content).expect("skill should parse");
        assert_eq!(skill.name, "rust-safety");
        assert_eq!(skill.description, "Safe Rust");
        assert_eq!(skill.content, "Avoid unsafe.");
    }

    #[test]
    fn parse_skill_uses_default_name_when_name_missing() {
        let content = "---\ndescription: Git workflow\n---\n\nCommit often.\n";
        let skill = parse_skill_file("git-workflow", content).expect("skill should parse");
        assert_eq!(skill.name, "git-workflow");
        assert_eq!(skill.description, "Git workflow");
    }

    #[test]
    fn parse_skill_returns_none_when_no_frontmatter() {
        let content = "No frontmatter here.";
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
