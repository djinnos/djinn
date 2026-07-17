//! Platform-owned native skill registry.
//!
//! Native skills are compiled into the `djinn-agent` artifact and cannot be
//! modified through project-provided skills or the user-editable agent skills
//! manifest. They are
//! immutable at runtime and carry an explicit version stamp per skill.
//!
//! The registry exposes lookup, listing, and role-recommendation helpers that
//! do **not** depend on `skill_path`, the skills manifest, or any filesystem
//! convention used by project skills.  Native skills can be converted to
//! [`ResolvedSkill`] for prompt formatting via [`NativeSkill::to_resolved`].

use crate::skills::ResolvedSkill;

// ─── Visual-spec version stamp ──────────────────────────────────────────────

/// Bump this constant when the embedded `visual-spec` content changes in a
/// way that downstream consumers (prompts, telemetry, lazy-loading caches)
/// should detect.  The value is exposed through [`native_skill_version`] and
/// embedded in the skill's metadata.
pub const VISUAL_SPEC_VERSION: &str = "1.2.0";

// ─── Embedded asset ─────────────────────────────────────────────────────────

/// Body of the `visual-spec` native skill, embedded at compile time from the
/// checked-in agent asset path.
const VISUAL_SPEC_CONTENT: &str = include_str!("native_assets/visual-spec/SKILL.md");

// ─── NativeSkill struct ─────────────────────────────────────────────────────

/// A single entry in the native skill registry.
///
/// Carries the same prompt-facing fields as [`ResolvedSkill`] plus a
/// `version` stamp that is absent from project skills.  The `trust_level`
/// is always `"platform"` to distinguish native skills from project-level
/// skills whose default trust level is `"project"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeSkill {
    /// Skill identifier (must be unique across the native registry).
    pub name: &'static str,
    /// Semver-ish version stamp — bump when the embedded content changes.
    pub version: &'static str,
    /// One-line description rendered in the skills section header.
    pub description: &'static str,
    /// Trust level — always `"platform"` for native skills.
    pub trust_level: &'static str,
    /// Roles that should receive this skill by default.
    pub recommended_for_roles: &'static [&'static str],
    /// Full embedded content (markdown body).
    pub content: &'static str,
}

impl NativeSkill {
    /// Convert to a [`ResolvedSkill`] for prompt formatting.
    ///
    /// The conversion is lossless for prompt purposes: every field that
    /// `ResolvedSkill` carries is populated.  The `tags` field is empty
    /// (native skills use roles instead of tags for targeting).
    pub fn to_resolved(&self) -> ResolvedSkill {
        ResolvedSkill {
            name: self.name.to_string(),
            description: self.description.to_string(),
            content: self.content.to_string(),
            // Native skills are always inlined — they are platform-authored
            // and must be present in the system prompt without an explicit
            // `skill_read` round-trip.
            required: true,
            trust_level: self.trust_level.to_string(),
            recommended_for_roles: self
                .recommended_for_roles
                .iter()
                .map(|s| s.to_string())
                .collect(),
            tags: Vec::new(),
        }
    }
}

// ─── Static registry ────────────────────────────────────────────────────────

/// All registered native skills, sorted by name.
///
/// To add a new native skill, append an entry here and bump the relevant
/// version constant.  Do **not** load entries from disk at runtime.
const NATIVE_SKILLS: &[NativeSkill] = &[NativeSkill {
    name: "visual-spec",
    version: VISUAL_SPEC_VERSION,
    description: "Proposal and plan authoring conventions: choosing the right \
                  MDX block for each kind of content, diagram/block quality, the \
                  bare-angle backtick constraint, and memory as the learned layer.",
    trust_level: "platform",
    // `planner` authors proposals during decomposition; `advocate` authors and
    // revises the proposal spec during tribunal refinement. Both produce
    // proposal MDX and need the visual-spec authoring conventions.
    recommended_for_roles: &["planner", "advocate"],
    content: VISUAL_SPEC_CONTENT,
}];

// ─── Lookup helpers ─────────────────────────────────────────────────────────

/// Look up a native skill by exact name.
///
/// Returns `None` when no native skill with that name exists.  This function
/// does **not** consult project skill paths or the skills manifest.
pub fn native_skill(name: &str) -> Option<&'static NativeSkill> {
    NATIVE_SKILLS.iter().find(|s| s.name == name)
}

/// Return the names of all registered native skills.
pub fn native_skill_names() -> Vec<&'static str> {
    NATIVE_SKILLS.iter().map(|s| s.name).collect()
}

/// Return native skill names recommended for `role`.
///
/// A skill is recommended for a role when its `recommended_for_roles` list
/// contains `role`.  The match is case-sensitive.
pub fn native_skill_names_for_role(role: &str) -> Vec<&'static str> {
    NATIVE_SKILLS
        .iter()
        .filter(|s| s.recommended_for_roles.contains(&role))
        .map(|s| s.name)
        .collect()
}

/// Return the version stamp for a native skill, or `None` if not found.
pub fn native_skill_version(name: &str) -> Option<&'static str> {
    NATIVE_SKILLS
        .iter()
        .find(|s| s.name == name)
        .map(|s| s.version)
}

/// Return all native skills recommended for `role`, converted to
/// [`ResolvedSkill`] for prompt formatting.
///
/// This is the primary integration point for session prompt construction:
/// call this function for the active role, then merge the result with
/// project-resolved skills.
pub fn resolved_native_skills_for_role(role: &str) -> Vec<ResolvedSkill> {
    NATIVE_SKILLS
        .iter()
        .filter(|s| s.recommended_for_roles.contains(&role))
        .map(|s| s.to_resolved())
        .collect()
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_skill_lookup_returns_visual_spec() {
        let skill = native_skill("visual-spec").expect("visual-spec should exist");
        assert_eq!(skill.name, "visual-spec");
        assert_eq!(skill.version, VISUAL_SPEC_VERSION);
        assert_eq!(skill.trust_level, "platform");
    }

    #[test]
    fn native_skill_lookup_returns_none_for_unknown() {
        assert!(native_skill("nonexistent-skill").is_none());
        assert!(native_skill("").is_none());
    }

    #[test]
    fn native_skill_names_contains_visual_spec() {
        let names = native_skill_names();
        assert!(
            names.contains(&"visual-spec"),
            "expected visual-spec in native skill names: {:?}",
            names,
        );
    }

    #[test]
    fn native_skill_version_matches_constant() {
        let version = native_skill_version("visual-spec").expect("version should exist");
        assert_eq!(version, VISUAL_SPEC_VERSION);
    }

    #[test]
    fn native_skill_version_none_for_unknown() {
        assert!(native_skill_version("no-such-skill").is_none());
    }

    #[test]
    fn visual_spec_recommended_for_planner() {
        let names = native_skill_names_for_role("planner");
        assert!(
            names.contains(&"visual-spec"),
            "expected visual-spec recommended for planner, got: {:?}",
            names,
        );
    }

    #[test]
    fn visual_spec_recommended_for_advocate() {
        // The tribunal Advocate authors/revises the proposal spec and must
        // receive the visual-spec authoring skill so refined specs are rich MDX.
        let names = native_skill_names_for_role("advocate");
        assert!(
            names.contains(&"visual-spec"),
            "expected visual-spec recommended for advocate, got: {:?}",
            names,
        );
    }

    #[test]
    fn visual_spec_not_recommended_for_worker() {
        let names = native_skill_names_for_role("worker");
        assert!(
            !names.contains(&"visual-spec"),
            "visual-spec should not be recommended for worker",
        );
    }

    #[test]
    fn visual_spec_not_recommended_for_reviewer() {
        let names = native_skill_names_for_role("reviewer");
        assert!(!names.contains(&"visual-spec"));
    }

    #[test]
    fn resolved_skill_conversion_preserves_metadata() {
        let skill = native_skill("visual-spec").unwrap();
        let resolved = skill.to_resolved();

        assert_eq!(resolved.name, "visual-spec");
        assert_eq!(resolved.trust_level, "platform");
        assert!(resolved.required);
        assert_eq!(resolved.recommended_for_roles, vec!["planner", "advocate"]);
        assert!(resolved.tags.is_empty());
        assert!(!resolved.description.is_empty());
        assert!(!resolved.content.is_empty());
    }

    #[test]
    fn resolved_native_skills_for_role_planner_returns_visual_spec() {
        let resolved = resolved_native_skills_for_role("planner");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "visual-spec");
    }

    #[test]
    fn resolved_native_skills_for_role_worker_returns_empty() {
        let resolved = resolved_native_skills_for_role("worker");
        assert!(resolved.is_empty());
    }

    #[test]
    fn visual_spec_content_includes_angle_bracket_constraint() {
        let skill = native_skill("visual-spec").unwrap();
        assert!(
            skill.content.contains("backtick"),
            "visual-spec content must mention the backtick constraint for bare angle brackets",
        );
        assert!(
            skill.content.contains('<') || skill.content.contains("angle"),
            "visual-spec content must address bare < / > angle bracket usage",
        );
    }

    #[test]
    fn visual_spec_content_includes_memory_as_learned_layer() {
        let skill = native_skill("visual-spec").unwrap();
        let lower = skill.content.to_lowercase();
        assert!(
            lower.contains("memory"),
            "visual-spec content must mention memory as the learned layer",
        );
        assert!(
            lower.contains("learned") || lower.contains("refinement"),
            "visual-spec content must teach memory as the learned/refinement layer",
        );
    }

    #[test]
    fn visual_spec_content_teaches_block_usage() {
        // The skill must map content kinds to concrete MDX blocks so the
        // advocate authors rich specs (FileTree/AnnotatedCode/Diagram) rather
        // than walls of prose.
        let skill = native_skill("visual-spec").unwrap();
        let lower = skill.content.to_lowercase();
        assert!(lower.contains("mdx"), "visual-spec must mention MDX");
        for block in ["filetree", "annotatedcode", "diagram", "callout"] {
            assert!(
                lower.contains(block),
                "visual-spec content must guide use of the {block} block",
            );
        }
    }

    #[test]
    fn visual_spec_content_includes_block_quality() {
        let skill = native_skill("visual-spec").unwrap();
        let lower = skill.content.to_lowercase();
        assert!(
            lower.contains("block"),
            "visual-spec content must address block authoring quality",
        );
    }

    #[test]
    fn native_skill_loading_independent_of_project_paths() {
        // Verify that the registry functions work without any filesystem
        // structure (.djinn, .claude, .opencode, or skills manifest).
        let names = native_skill_names();
        assert!(!names.is_empty());

        let skill = native_skill(names[0]).unwrap();
        let resolved = skill.to_resolved();
        assert!(!resolved.content.is_empty());
    }
}
