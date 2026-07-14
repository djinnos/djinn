
use super::*;
use crate::test_helpers::test_tempdir;
use std::fs;

fn write_flat_skill(dir: &Path, name: &str, body: &str) {
    fs::write(dir.join(format!("{name}.md")), body).unwrap();
}

fn djinn_skills_dir(project_root: &Path) -> PathBuf {
    let dir = project_root.join(".djinn").join("skills");
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_with_references(skills_dir: &Path, name: &str, body: &str, references: &[(&str, &str)]) {
    let skill_dir = skills_dir.join(name);
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(skill_dir.join("SKILL.md"), body).unwrap();
    let refs = skill_dir.join("references");
    fs::create_dir_all(&refs).unwrap();
    for (rel, content) in references {
        let target = refs.join(rel);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(target, content).unwrap();
    }
}

fn write_checked_manifest(project_root: &Path) {
    let manifest_path = project_root.join(DEFAULT_MANIFEST_PATH);
    fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
    let manifest = generate_manifest(project_root, None).unwrap();
    fs::write(&manifest_path, to_pretty_json(&manifest).unwrap()).unwrap();
}

#[test]
fn generate_manifest_is_deterministic_across_runs() {
    let tmp = test_tempdir("djinn-skills-manifest-");
    let skills_dir = djinn_skills_dir(tmp.path());

    write_flat_skill(
        &skills_dir,
        "alpha",
        "---\nname: alpha\ndescription: First\n---\n\nBody A.\n",
    );
    write_flat_skill(
        &skills_dir,
        "beta",
        "---\nname: beta\ndescription: Second\n---\n\nBody B.\n",
    );

    let first = generate_manifest(tmp.path(), None).expect("generate first manifest");
    let second = generate_manifest(tmp.path(), None).expect("generate second manifest");
    assert_eq!(first, second, "manifest must be byte-stable across runs");
    assert_eq!(first.skills.len(), 2);
    assert_eq!(first.skills[0].id, "alpha");
    assert_eq!(first.skills[1].id, "beta");
}

#[test]
fn generate_manifest_summary_hash_covers_name_description_required() {
    let tmp = test_tempdir("djinn-skills-summary-");
    let skills_dir = djinn_skills_dir(tmp.path());

    write_flat_skill(
        &skills_dir,
        "rust-safety",
        "---\nname: rust-safety\ndescription: Safe Rust\nrequired: true\n---\n\nAvoid unsafe.\n",
    );

    let manifest = generate_manifest(tmp.path(), None).unwrap();
    let skill = &manifest.skills[0];

    let mut h1 = Sha256::new();
    h1.update(format!(
        "name={}\ndescription={}\nrequired={}",
        skill.name, skill.description, skill.required
    ));
    let expected = format!("sha256:{:x}", h1.finalize());
    assert_eq!(skill.summary_hash, expected);

    // Flipping `required` must change the summary hash.
    write_flat_skill(
        &skills_dir,
        "rust-safety",
        "---\nname: rust-safety\ndescription: Safe Rust\nrequired: false\n---\n\nAvoid unsafe.\n",
    );
    let manifest2 = generate_manifest(tmp.path(), None).unwrap();
    assert_ne!(
        manifest2.skills[0].summary_hash, skill.summary_hash,
        "summary_hash must respond to required flag"
    );
}

#[test]
fn manifest_hashes_cover_progressive_disclosure_and_skill_read_body_cases() {
    let tmp = test_tempdir("djinn-skills-progressive-manifest-");
    let skills_dir = djinn_skills_dir(tmp.path());

    write_flat_skill(
        &skills_dir,
        "required-rules",
        "---\nname: required-rules\ndescription: Required house rules\nrequired: true\n---\n\nRequired full body.\n",
    );
    write_flat_skill(
        &skills_dir,
        "ondemand-rust",
        "---\nname: ondemand-rust\ndescription: Optional Rust guidance\nrequired: false\n---\n\nOn-demand full body.\n",
    );

    let manifest = generate_manifest(tmp.path(), None).unwrap();
    assert_eq!(manifest.skills.len(), 2);
    fs::create_dir_all(tmp.path().join(".djinn")).unwrap();
    fs::write(
        tmp.path().join(DEFAULT_MANIFEST_PATH),
        to_pretty_json(&manifest).unwrap(),
    )
    .unwrap();

    let loaded = load_verified_skills(
        tmp.path(),
        &["required-rules".to_string(), "ondemand-rust".to_string()],
    )
    .expect("fresh manifest should verify for prompt/skill_read runtime path");
    let section = crate::skills::format_skills_section_with(&loaded, true);

    assert!(section.contains("**required-rules**: Required house rules"));
    assert!(section.contains("Required full body."));
    assert!(!section.contains("skill_read(name=\"required-rules\")"));

    assert!(section.contains("**ondemand-rust**: Optional Rust guidance"));
    assert!(section.contains("skill_read(name=\"ondemand-rust\")"));
    assert!(
        !section.contains("On-demand full body."),
        "non-required body must stay out of progressive disclosure prompt"
    );

    for skill in &loaded {
        let entry = manifest
            .skills
            .iter()
            .find(|entry| entry.id == skill.name)
            .expect("manifest entry for loaded skill");
        assert_eq!(entry.required, skill.required);
        assert_eq!(entry.description, skill.description);
        assert_eq!(
            entry.summary_hash,
            sha256_prefixed(summary_bytes(skill).as_bytes())
        );
        assert_eq!(
            entry.content_hash,
            sha256_prefixed(skill.content.as_bytes())
        );
    }

    // A stale materialized summary field must be caught before prompt
    // rendering can disclose the wrong name/description/required tuple.
    write_flat_skill(
        &skills_dir,
        "ondemand-rust",
        "---\nname: ondemand-rust\ndescription: Tampered optional guidance\nrequired: false\n---\n\nOn-demand full body.\n",
    );
    let err = load_verified_skills(tmp.path(), &["ondemand-rust".to_string()])
        .expect_err("summary tamper must fail the runtime verification path");
    assert!(
        err.to_string().contains("description") || err.to_string().contains("summary_hash"),
        "got: {err}"
    );
}

#[test]
fn generate_manifest_content_hash_covers_served_body_with_references() {
    let tmp = test_tempdir("djinn-skills-content-");
    let skills_dir = djinn_skills_dir(tmp.path());

    write_with_references(
        &skills_dir,
        "ref-skill",
        "---\ndescription: Skill with references\n---\n\nPrimary body.\n",
        &[
            ("z-last.md", "Zed reference\n"),
            ("a-first.md", "Alpha reference\n"),
            ("nested/b-middle.md", "Nested reference\n"),
        ],
    );

    let manifest = generate_manifest(tmp.path(), None).unwrap();
    let skill = &manifest.skills[0];
    assert_eq!(skill.id, "ref-skill");

    // The content_hash must equal sha256 of the *effective* body that
    // `load_skills` would serve — primary body + sorted references.
    let loaded = crate::skills::load_skills(tmp.path(), &["ref-skill".to_string()]);
    assert_eq!(loaded.len(), 1);
    let mut h = Sha256::new();
    h.update(loaded[0].content.as_bytes());
    let expected = format!("sha256:{:x}", h.finalize());
    assert_eq!(skill.content_hash, expected);
}

#[test]
fn generate_manifest_records_per_file_sha256_in_sorted_order() {
    let tmp = test_tempdir("djinn-skills-files-");
    let skills_dir = djinn_skills_dir(tmp.path());

    write_with_references(
        &skills_dir,
        "ordered-skill",
        "---\ndescription: Sort test\n---\n\nPrimary.\n",
        &[("z.md", "Z\n"), ("a.md", "A\n"), ("nested/b.md", "B\n")],
    );

    let manifest = generate_manifest(tmp.path(), None).unwrap();
    let skill = &manifest.skills[0];

    // Paths are sorted lexicographically.
    let paths: Vec<&str> = skill
        .source_files
        .iter()
        .map(|sf| sf.path.as_str())
        .collect();
    assert_eq!(
        paths,
        vec![
            ".djinn/skills/ordered-skill/SKILL.md",
            ".djinn/skills/ordered-skill/references/a.md",
            ".djinn/skills/ordered-skill/references/nested/b.md",
            ".djinn/skills/ordered-skill/references/z.md",
        ]
    );

    // First entry is the top-level skill, the rest are references.
    assert_eq!(skill.source_files[0].role, ManifestSourceRole::Skill);
    assert!(
        skill
            .source_files
            .iter()
            .skip(1)
            .all(|sf| sf.role == ManifestSourceRole::Reference)
    );

    // Each sha256 is the hex of the file bytes.
    for entry in &skill.source_files {
        let absolute = tmp.path().join(&entry.path);
        let bytes = fs::read(&absolute).unwrap();
        let mut h = Sha256::new();
        h.update(&bytes);
        let expected = format!("{:x}", h.finalize());
        assert_eq!(entry.sha256, expected);
    }
}

#[test]
fn generate_manifest_uses_safe_defaults_when_metadata_omitted() {
    let tmp = test_tempdir("djinn-skills-defaults-");
    let skills_dir = djinn_skills_dir(tmp.path());

    write_flat_skill(
        &skills_dir,
        "legacy",
        "---\ndescription: No metadata\n---\n\nBody.\n",
    );

    let manifest = generate_manifest(tmp.path(), None).unwrap();
    let skill = &manifest.skills[0];
    assert_eq!(skill.trust_level, "project");
    assert!(skill.recommended_for_roles.is_empty());
    assert!(skill.tags.is_empty());
    assert!(!skill.required);
}

#[test]
fn generate_manifest_reads_optional_metadata_keys() {
    let tmp = test_tempdir("djinn-skills-meta-");
    let skills_dir = djinn_skills_dir(tmp.path());

    write_flat_skill(
        &skills_dir,
        "annotated",
        "---\ndescription: Annotated\ntrust_level: trusted\nrecommended_for_roles: [worker, reviewer]\ntags: [alpha, beta]\n---\n\nBody.\n",
    );

    let manifest = generate_manifest(tmp.path(), None).unwrap();
    let skill = &manifest.skills[0];
    assert_eq!(skill.trust_level, "trusted");
    assert_eq!(skill.recommended_for_roles, vec!["worker", "reviewer"]);
    assert_eq!(skill.tags, vec!["alpha", "beta"]);
}

#[test]
fn generate_manifest_empty_when_no_skills_present() {
    let tmp = test_tempdir("djinn-skills-empty-");
    // No skill directories created.
    let manifest = generate_manifest(tmp.path(), None).unwrap();
    assert!(manifest.skills.is_empty());
    assert_eq!(manifest.schema_version, MANIFEST_SCHEMA_VERSION);
    assert_eq!(manifest.generated_by, MANIFEST_GENERATED_BY);
}

#[test]
fn verify_manifest_succeeds_against_unchanged_files() {
    let tmp = test_tempdir("djinn-skills-verify-ok-");
    let skills_dir = djinn_skills_dir(tmp.path());
    write_with_references(
        &skills_dir,
        "good",
        "---\ndescription: Stable\n---\n\nPrimary.\n",
        &[("a.md", "A\n"), ("nested/b.md", "B\n")],
    );

    let manifest = generate_manifest(tmp.path(), None).unwrap();
    verify_manifest(tmp.path(), &manifest).expect("verify should pass");
}

#[test]
fn verify_manifest_detects_tampered_source_file() {
    let tmp = test_tempdir("djinn-skills-verify-tamper-");
    let skills_dir = djinn_skills_dir(tmp.path());
    write_flat_skill(
        &skills_dir,
        "tamper",
        "---\ndescription: Original\n---\n\nOriginal body.\n",
    );

    let manifest = generate_manifest(tmp.path(), None).unwrap();
    verify_manifest(tmp.path(), &manifest).expect("verify should pass before tamper");

    // Tamper with the body — content_hash must flag the mismatch.
    fs::write(
        skills_dir.join("tamper.md"),
        "---\ndescription: Original\n---\n\nModified body.\n",
    )
    .unwrap();
    let err =
        verify_manifest(tmp.path(), &manifest).expect_err("verify must fail on tampered file");
    assert!(
        matches!(err, ManifestError::ContentHashMismatch { .. }),
        "expected ContentHashMismatch, got {err:?}"
    );
}

#[test]
fn verify_manifest_detects_tampered_reference_file() {
    let tmp = test_tempdir("djinn-skills-verify-ref-tamper-");
    let skills_dir = djinn_skills_dir(tmp.path());
    write_with_references(
        &skills_dir,
        "ref-tamper",
        "---\ndescription: Stable\n---\n\nPrimary.\n",
        &[("a.md", "Original\n")],
    );

    let manifest = generate_manifest(tmp.path(), None).unwrap();
    verify_manifest(tmp.path(), &manifest).expect("verify should pass before tamper");

    // Tamper with the reference file directly.
    fs::write(
        skills_dir
            .join("ref-tamper")
            .join("references")
            .join("a.md"),
        "Modified\n",
    )
    .unwrap();
    let err = verify_manifest(tmp.path(), &manifest)
        .expect_err("verify must fail on tampered reference file");
    assert!(
        matches!(
            err,
            ManifestError::SourceHashMismatch { .. } | ManifestError::ContentHashMismatch { .. }
        ),
        "expected per-file or content hash mismatch, got {err:?}"
    );
}

#[test]
fn verify_manifest_detects_changed_required_flag() {
    let tmp = test_tempdir("djinn-skills-verify-required-");
    let skills_dir = djinn_skills_dir(tmp.path());
    write_flat_skill(
        &skills_dir,
        "req",
        "---\ndescription: Required toggle\nrequired: false\n---\n\nBody.\n",
    );

    let manifest = generate_manifest(tmp.path(), None).unwrap();
    verify_manifest(tmp.path(), &manifest).expect("verify should pass before flip");

    fs::write(
        skills_dir.join("req.md"),
        "---\ndescription: Required toggle\nrequired: true\n---\n\nBody.\n",
    )
    .unwrap();
    let err = verify_manifest(tmp.path(), &manifest)
        .expect_err("verify must fail when required flag flips");
    assert!(
        matches!(
            err,
            ManifestError::MetadataMismatch {
                field: "required",
                ..
            }
        ),
        "expected required MetadataMismatch, got {err:?}"
    );
}

#[test]
fn load_verified_skills_allows_missing_manifest_for_legacy_projects() {
    let tmp = test_tempdir("djinn-skills-runtime-missing-manifest-");
    let skills_dir = djinn_skills_dir(tmp.path());
    write_flat_skill(
        &skills_dir,
        "legacy",
        "---\ndescription: No manifest\n---\n\nBody.\n",
    );

    let loaded = load_verified_skills(tmp.path(), &["legacy".to_string()])
        .expect("missing manifest is accepted for legacy projects");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].content, "Body.");
}

#[test]
fn load_verified_skills_rejects_tampered_served_body_when_manifest_exists() {
    let tmp = test_tempdir("djinn-skills-runtime-tamper-");
    let skills_dir = djinn_skills_dir(tmp.path());
    write_flat_skill(
        &skills_dir,
        "tamper",
        "---\ndescription: Runtime tamper\n---\n\nOriginal body.\n",
    );
    write_checked_manifest(tmp.path());

    write_flat_skill(
        &skills_dir,
        "tamper",
        "---\ndescription: Runtime tamper\n---\n\nModified body.\n",
    );

    let err = load_verified_skills(tmp.path(), &["tamper".to_string()])
        .expect_err("runtime verification must reject stale/tampered body");
    let message = err.to_string();
    assert!(message.contains("skills manifest verification failed"));
    assert!(message.contains("content_hash"), "got: {message}");
}

#[test]
fn load_verified_skills_rejects_loaded_skill_not_in_manifest() {
    let tmp = test_tempdir("djinn-skills-runtime-unlisted-");
    let skills_dir = djinn_skills_dir(tmp.path());
    write_flat_skill(
        &skills_dir,
        "manifested",
        "---\ndescription: Manifested\n---\n\nBody.\n",
    );
    write_checked_manifest(tmp.path());
    write_flat_skill(
        &skills_dir,
        "unlisted",
        "---\ndescription: Unlisted\n---\n\nBody.\n",
    );

    let err = load_verified_skills(tmp.path(), &["unlisted".to_string()])
        .expect_err("manifested projects must not silently serve unlisted skills");
    assert!(
        err.to_string()
            .contains("not present in the checked skills manifest"),
        "got: {err}"
    );
}

#[test]
fn check_manifest_drift_detects_skill_file_changed_without_regeneration() {
    let tmp = test_tempdir("djinn-skills-drift-skill-");
    let skills_dir = djinn_skills_dir(tmp.path());
    write_flat_skill(
        &skills_dir,
        "drifty",
        "---\ndescription: Drift check\n---\n\nOriginal body.\n",
    );
    let manifest_path = tmp.path().join(DEFAULT_MANIFEST_PATH);
    fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
    let manifest = generate_manifest(tmp.path(), None).unwrap();
    fs::write(&manifest_path, to_pretty_json(&manifest).unwrap()).unwrap();

    check_manifest_drift(tmp.path(), &manifest_path).expect("fresh manifest should pass");

    write_flat_skill(
        &skills_dir,
        "drifty",
        "---\ndescription: Drift check\n---\n\nModified body.\n",
    );
    let err = check_manifest_drift(tmp.path(), &manifest_path)
        .expect_err("changed skill without regenerate must fail");
    let message = err.to_string();
    assert!(message.contains("skills manifest drift detected"));
    assert!(message.contains("make skills-manifest-generate"));
}

#[test]
fn check_manifest_drift_detects_reference_file_changed_without_regeneration() {
    let tmp = test_tempdir("djinn-skills-drift-reference-");
    let skills_dir = djinn_skills_dir(tmp.path());
    write_with_references(
        &skills_dir,
        "ref-drifty",
        "---\ndescription: Reference drift check\n---\n\nPrimary.\n",
        &[("a.md", "Original reference\n")],
    );
    let manifest_path = tmp.path().join(DEFAULT_MANIFEST_PATH);
    fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
    let manifest = generate_manifest(tmp.path(), None).unwrap();
    fs::write(&manifest_path, to_pretty_json(&manifest).unwrap()).unwrap();

    fs::write(
        skills_dir
            .join("ref-drifty")
            .join("references")
            .join("a.md"),
        "Modified reference\n",
    )
    .unwrap();

    let err = check_manifest_drift(tmp.path(), &manifest_path)
        .expect_err("changed reference without regenerate must fail");
    assert!(err.to_string().contains("make skills-manifest-generate"));
}

#[test]
fn manifest_round_trips_through_json() {
    let tmp = test_tempdir("djinn-skills-roundtrip-");
    let skills_dir = djinn_skills_dir(tmp.path());
    write_with_references(
        &skills_dir,
        "rt",
        "---\ndescription: Round trip\ntrust_level: trusted\nrecommended_for_roles: [worker]\ntags: [a]\n---\n\nBody.\n",
        &[("a.md", "A\n")],
    );

    let manifest = generate_manifest(tmp.path(), None).unwrap();
    let json = to_pretty_json(&manifest).expect("serialize");
    let parsed: SkillsManifest = serde_json::from_str(&json).expect("parse");
    assert_eq!(manifest, parsed);
    // Re-verify against the re-parsed manifest to prove JSON didn't drop
    // hash bytes silently.
    verify_manifest(tmp.path(), &parsed).expect("verify after roundtrip");
}

#[test]
fn summary_bytes_is_stable_across_canonical_inputs() {
    let skill = ResolvedSkill {
        name: "n".to_string(),
        description: "d".to_string(),
        content: "c".to_string(),
        required: true,
        trust_level: "project".to_string(),
        recommended_for_roles: vec![],
        tags: vec![],
    };
    assert_eq!(
        summary_bytes(&skill),
        "name=n\ndescription=d\nrequired=true"
    );
}

#[test]
fn detailed_verified_load_reports_missing_declared_skill_and_reference() {
    let tmp = test_tempdir("djinn-skills-detailed-missing-");
    let skills_dir = djinn_skills_dir(tmp.path());
    write_with_references(
        &skills_dir,
        "referenced",
        "---\ndescription: Reference skill\n---\n\nBody.\n",
        &[("guide.md", "Guide\n")],
    );
    write_checked_manifest(tmp.path());

    fs::remove_file(
        skills_dir
            .join("referenced")
            .join("references")
            .join("guide.md"),
    )
    .unwrap();
    let detailed = load_verified_skills_detailed(tmp.path(), &["referenced".to_string()]);
    assert!(detailed.error.is_some());
    assert_eq!(detailed.diagnostics.len(), 1);
    assert_eq!(detailed.diagnostics[0].source_key, "referenced");
    assert_eq!(
        detailed.diagnostics[0].phase,
        djinn_core::extension_diagnostics::ExtensionLoadPhase::MissingFile
    );
    assert_eq!(
        detailed.diagnostics[0].remedy_code,
        djinn_core::extension_diagnostics::ExtensionLoadRemedyCode::RestoreSkillFile
    );

    fs::remove_dir_all(skills_dir.join("referenced")).unwrap();
    let detailed = load_verified_skills_detailed(tmp.path(), &["referenced".to_string()]);
    assert!(detailed.error.is_some());
    assert_eq!(detailed.diagnostics.len(), 1);
    assert_eq!(
        detailed.diagnostics[0].phase,
        djinn_core::extension_diagnostics::ExtensionLoadPhase::MissingFile
    );
}

#[test]
fn detailed_verified_load_reports_manifest_drift_without_payloads() {
    let tmp = test_tempdir("djinn-skills-detailed-drift-");
    let skills_dir = djinn_skills_dir(tmp.path());
    write_flat_skill(
        &skills_dir,
        "checked",
        "---\ndescription: Stable\n---\n\nBody.\n",
    );
    write_checked_manifest(tmp.path());
    write_flat_skill(
        &skills_dir,
        "checked",
        "---\ndescription: Changed\n---\n\nBody.\n",
    );

    let detailed = load_verified_skills_detailed(tmp.path(), &["checked".to_string()]);
    assert!(detailed.error.is_some());
    assert_eq!(detailed.diagnostics.len(), 1);
    let fact = &detailed.diagnostics[0];
    assert_eq!(fact.source_key, "checked");
    assert_eq!(
        fact.phase,
        djinn_core::extension_diagnostics::ExtensionLoadPhase::ManifestDrift
    );
    assert_eq!(
        fact.remedy_code,
        djinn_core::extension_diagnostics::ExtensionLoadRemedyCode::UpdateSkillManifest
    );
    assert!(!fact.summary_material.contains(tmp.path().to_str().unwrap()));
    assert!(!fact.summary_material.contains("Changed"));
}

#[test]
fn detailed_verified_load_reports_only_frontmatter_for_malformed_declared_skill() {
    let tmp = test_tempdir("djinn-skills-detailed-frontmatter-");
    let skills_dir = djinn_skills_dir(tmp.path());
    write_flat_skill(
        &skills_dir,
        "declared",
        "---\ndescription: Valid before manifest generation\n---\n\nBody.\n",
    );
    write_checked_manifest(tmp.path());
    write_flat_skill(&skills_dir, "declared", "not frontmatter\n");

    let detailed = load_verified_skills_detailed(tmp.path(), &["declared".to_string()]);

    assert!(
        detailed.error.is_some(),
        "manifest verification remains fail-closed"
    );
    assert_eq!(detailed.skills.len(), 0);
    assert_eq!(detailed.diagnostics.len(), 1);
    let fact = &detailed.diagnostics[0];
    assert_eq!(fact.source_key, "declared");
    assert_eq!(
        fact.phase,
        djinn_core::extension_diagnostics::ExtensionLoadPhase::Frontmatter
    );
    assert_eq!(
        fact.remedy_code,
        djinn_core::extension_diagnostics::ExtensionLoadRemedyCode::CheckSkillFrontmatter
    );
}

#[test]
fn detailed_verified_load_never_diagnoses_native_skill_requests() {
    let tmp = test_tempdir("djinn-skills-detailed-native-");
    write_checked_manifest(tmp.path());

    let detailed = load_verified_skills_detailed(tmp.path(), &["visual-spec".to_string()]);

    assert!(detailed.error.is_none());
    assert!(detailed.skills.is_empty());
    assert!(detailed.diagnostics.is_empty());
}

#[test]
fn relative_posix_path_handles_nested_skill_files() {
    let tmp = test_tempdir("djinn-skills-posix-");
    let nested = tmp.path().join(".djinn/skills/foo/references");
    fs::create_dir_all(&nested).unwrap();
    let file = nested.join("a.md");
    fs::write(&file, "x").unwrap();
    let rel = relative_posix_path(tmp.path(), &file).expect("relative");
    assert_eq!(rel, ".djinn/skills/foo/references/a.md");
}
