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
            load_skill(&path, name)
        })
        .collect()
}

fn load_skill(path: &Path, default_name: &str) -> Option<ResolvedSkill> {
    let content = fs::read_to_string(path).ok()?;
    let references = skill_references_content(path);
    parse_skill_file(default_name, &content, references.as_deref())
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

fn collect_reference_files(root: &Path, current_dir: &Path, files: &mut Vec<(PathBuf, PathBuf)>) {
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
