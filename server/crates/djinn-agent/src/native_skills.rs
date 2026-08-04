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

/// Version of the platform-owned readiness guardrail catalog.
pub const AGENT_READINESS_GUARDRAILS_VERSION: &str = "1.1.0";

// ─── Embedded asset ─────────────────────────────────────────────────────────

/// Body of the `visual-spec` native skill, embedded at compile time from the
/// checked-in agent asset path.
const VISUAL_SPEC_CONTENT: &str = include_str!("native_assets/visual-spec/SKILL.md");

/// Body of the `agent-readiness-guardrails` native skill, embedded from the
/// checked-in platform catalog rather than a project-provided skill path.
const AGENT_READINESS_GUARDRAILS_CONTENT: &str =
    include_str!("native_assets/agent-readiness-guardrails/SKILL.md");

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

/// Failure returned by an exact native-skill pin lookup.
///
/// Callers that receive a name and version from persisted task context must use
/// this instead of falling back to a same-name entry at another version.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NativeSkillLookupError {
    #[error("unknown native skill `{name}`")]
    UnknownName { name: String },
    #[error(
        "native skill `{name}` is registered at version `{registered_version}`, not requested version `{requested_version}`"
    )]
    VersionMismatch {
        name: String,
        requested_version: String,
        registered_version: String,
    },
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
const NATIVE_SKILLS: &[NativeSkill] = &[
    NativeSkill {
        name: "agent-readiness-guardrails",
        version: AGENT_READINESS_GUARDRAILS_VERSION,
        description: "Evidence-oriented S0 guardrails for Architect readiness analysis.",
        trust_level: "platform",
        // This is discovery metadata only. Runtime eligibility requires the
        // typed readiness context and an exact version pin.
        recommended_for_roles: &["architect"],
        content: AGENT_READINESS_GUARDRAILS_CONTENT,
    },
    NativeSkill {
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
    },
];

// ─── Lookup helpers ─────────────────────────────────────────────────────────

/// Look up a native skill by exact name.
///
/// Returns `None` when no native skill with that name exists.  This function
/// does **not** consult project skill paths or the skills manifest.
pub fn native_skill(name: &str) -> Option<&'static NativeSkill> {
    NATIVE_SKILLS.iter().find(|s| s.name == name)
}

/// Look up a native skill using its exact registered `(name, version)` pin.
///
/// This deliberately reports a same-name version mismatch instead of silently
/// returning a newer or older embedded catalog.
pub fn native_skill_exact(
    name: &str,
    version: &str,
) -> Result<&'static NativeSkill, NativeSkillLookupError> {
    let skill = native_skill(name).ok_or_else(|| NativeSkillLookupError::UnknownName {
        name: name.to_string(),
    })?;
    if skill.version != version {
        return Err(NativeSkillLookupError::VersionMismatch {
            name: name.to_string(),
            requested_version: version.to_string(),
            registered_version: skill.version.to_string(),
        });
    }
    Ok(skill)
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
    use std::collections::{HashMap, HashSet};

    const CI_TIMEOUT_RUNTIME_EVIDENCE_FIXTURE: &str = include_str!(
        "native_assets/agent-readiness-guardrails/fixtures/ci-timeout-runtime-evidence.json"
    );

    fn provider_key(job: &serde_json::Value) -> String {
        let mut labels = job["runner_labels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|label| label.as_str().unwrap())
            .collect::<Vec<_>>();
        labels.sort_unstable();
        format!(
            "{}|{}|{}|{}",
            job["workflow_path"].as_str().unwrap(),
            job["rendered_job_name"].as_str().unwrap(),
            labels.join(","),
            job["runner_group_id"].as_str().unwrap(),
        )
    }

    fn valid_durations(jobs: &[serde_json::Value], key: &str, assessed_at: i64) -> Vec<i64> {
        let mut run_key_provider_ids = HashMap::new();
        for job in jobs.iter().filter(|job| job["run_attempt"] == 1) {
            run_key_provider_ids
                .entry((job["workflow_run_id"].as_str().unwrap(), provider_key(job)))
                .or_insert_with(HashSet::new)
                .insert(job["provider_job_id"].as_str().unwrap());
        }
        let mut seen = HashSet::new();
        let mut durations = Vec::new();
        for job in jobs {
            let run_id = job["workflow_run_id"].as_str().unwrap();
            if provider_key(job) != key
                || job["run_attempt"] != 1
                || job["conclusion"] != "success"
                || run_key_provider_ids[&(run_id, provider_key(job))].len() != 1
            {
                continue;
            }
            let (Some(started), Some(completed)) = (
                job["started_at_unix"].as_i64(),
                job["completed_at_unix"].as_i64(),
            ) else {
                continue;
            };
            if assessed_at - completed > 30 * 24 * 60 * 60 || completed <= started {
                continue;
            }
            if seen.insert((run_id, job["run_attempt"].as_i64(), job["provider_job_id"].as_str())) {
                durations.push(completed - started);
            }
        }
        durations.sort_unstable();
        durations
    }

    #[test]
    fn readiness_guardrails_are_registered_as_a_platform_architect_recommendation() {
        let skill = native_skill_exact("agent-readiness-guardrails", "1.1.0")
            .expect("registered readiness pin should resolve");
        assert_eq!(skill.version, AGENT_READINESS_GUARDRAILS_VERSION);
        assert_eq!(skill.trust_level, "platform");
        assert_eq!(skill.recommended_for_roles, &["architect"]);
        assert!(skill.to_resolved().required);
    }

    #[test]
    fn exact_lookup_rejects_unknown_names_and_version_substitution() {
        assert_eq!(
            native_skill_exact("not-registered", "1.0.0"),
            Err(NativeSkillLookupError::UnknownName {
                name: "not-registered".to_string(),
            })
        );
        assert_eq!(
            native_skill_exact("agent-readiness-guardrails", "1.0.0"),
            Err(NativeSkillLookupError::VersionMismatch {
                name: "agent-readiness-guardrails".to_string(),
                requested_version: "1.0.0".to_string(),
                registered_version: "1.1.0".to_string(),
            })
        );
    }

    #[test]
    fn readiness_catalog_has_exactly_the_nine_s0_guardrails_including_ci_timeouts() {
        let skill = native_skill("agent-readiness-guardrails").unwrap();
        for id in [
            "GOV-MIG-001",
            "EVID-ACCEPTANCE-001",
            "EVID-SMOKE-001",
            "EVID-SELECTOR-001",
            "CONTRACT-API-001",
            "TEST-DB-001",
            "OBS-BASELINE-001",
            "SUPPLY-CHAIN-001",
            "CI-TIMEOUT-001",
        ] {
            assert!(skill.content.contains(id), "catalog must include {id}");
        }
        assert_eq!(skill.content.matches("### Guardrail:").count(), 9);
        for required_contract_text in [
            ".github/ci-timeouts.json",
            "scripts/check-ci-timeouts.mjs",
            "required-context-inventory-unverified",
            "GET /repos/{owner}/{repo}/actions/workflows",
            "GET /repos/{owner}/{repo}/actions/workflows/{workflow_id}/runs",
            "GET /repos/{owner}/{repo}/actions/runs/{run_id}/attempts/1/jobs",
            "runner_group_id",
            "normalized/sorted runner labels",
            "run_attempt != 1",
            "ambiguous-provider-job-identity",
            "job-requires-partitioning",
            "Coverage-compliance outcome",
            "Recommendation-confidence outcome",
            "source URL and retrieval timestamp",
            "configured timeout",
            "recommended timeout",
            "ceil(0.95 * n)",
            "ceil(1.5 * p95_seconds / 60)",
        ] {
            assert!(
                skill.content.contains(required_contract_text),
                "CI timeout contract must include {required_contract_text}",
            );
        }
        assert!(
            !skill
                .content
                .to_lowercase()
                .contains("code-graph guardrail")
        );
        assert!(
            !skill
                .content
                .to_lowercase()
                .contains("complexity guardrail")
        );
    }

    #[test]
    fn ci_timeout_runtime_fixture_proves_advisory_assessment_contract() {
        let fixture: serde_json::Value = serde_json::from_str(CI_TIMEOUT_RUNTIME_EVIDENCE_FIXTURE)
            .expect("checked-in CI timeout fixture must be valid JSON");
        let jobs = fixture["jobs"].as_array().unwrap();
        let fresh = fixture["assessment_times"]["fresh_unix"].as_i64().unwrap();
        let aged = fixture["assessment_times"]["aged_unix"].as_i64().unwrap();
        let stable_key = provider_key(&jobs[0]);

        let durations = valid_durations(jobs, &stable_key, fresh);
        assert_eq!(durations, vec![300, 330, 360, 390, 420, 450, 480, 510, 540, 600]);
        let p95 = durations[((95 * durations.len() + 99) / 100) - 1];
        let recommendation = ((3 * p95 + 119) / 120).clamp(5, 120);
        assert_eq!((p95, recommendation), (600, 15));

        let canary = provider_key(&jobs[10]);
        let nightly = provider_key(&jobs[11]);
        assert_ne!(canary, nightly, "exact rendered matrix names must remain separate");
        assert_eq!(valid_durations(jobs, &canary, fresh).len(), 1);
        assert_eq!(valid_durations(jobs, &nightly, fresh).len(), 1);

        let collisions = jobs
            .iter()
            .filter(|job| job["workflow_run_id"] == "collision-01")
            .collect::<Vec<_>>();
        assert_eq!(collisions.len(), 2);
        assert_eq!(provider_key(collisions[0]), provider_key(collisions[1]));
        assert_eq!(valid_durations(jobs, &stable_key, fresh).len(), 10);
        let duplicate_key = provider_key(&jobs[22]);
        assert_eq!(valid_durations(jobs, &duplicate_key, fresh).len(), 1);
        assert!(jobs.iter().any(|job| job["run_attempt"] != 1));

        for run_id in [
            "missing-time",
            "non-positive",
            "failed",
            "cancelled",
            "skipped",
            "neutral",
            "timed-out",
            "duplicate",
        ] {
            assert!(jobs.iter().any(|job| job["workflow_run_id"] == run_id));
        }
        assert!(valid_durations(jobs, &canary, fresh).len() < 10);

        assert!(valid_durations(jobs, &stable_key, aged).is_empty());
        assert_eq!(fixture["offline_checker"]["result"], "pass");
        assert!(fixture["source"].as_str().unwrap().contains("no live API request"));
    }

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
