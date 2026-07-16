//! Canonical verify profile resolution and exact-diff freshness evaluation.
//!
//! This module provides the pure, testable helper logic for the auto-submit
//! pipeline:
//!
//! 1. **Profile resolution** selects the authoritative canonical verify profile
//!    from (in precedence order): task metadata override → project verify
//!    configuration → repository default profile.
//!
//! 2. **Freshness evaluation** validates that a canonical verify run covers the
//!    current exact diff fingerprint, that the result is a pass, and that no
//!    tracked or allowed-untracked files have changed since the verify completed.
//!
//! This module does **not** trigger auto-submit — it is a pure helper consumed
//! by the decision gate (task 4sxe) and the wiring layer (task sq5h).

use serde::{Deserialize, Serialize};

use crate::models::{VerifyResult, VerifyRunRecord, VerifySource};

// ─── Profile resolution types ────────────────────────────────────────────────

/// Where the resolved canonical verify profile came from.
///
/// Used to populate review metadata so auditors know which source was
/// authoritative for a given auto-submit decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyProfileSource {
    /// Task-level metadata override (highest precedence).
    TaskMetadata,
    /// Project-level verify configuration.
    ProjectConfig,
    /// Repository-level default profile (lowest precedence).
    RepositoryDefault,
}

impl VerifyProfileSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::TaskMetadata => "task_metadata",
            Self::ProjectConfig => "project_config",
            Self::RepositoryDefault => "repository_default",
        }
    }
}

impl std::fmt::Display for VerifyProfileSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for VerifyProfileSource {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "task_metadata" => Ok(Self::TaskMetadata),
            "project_config" => Ok(Self::ProjectConfig),
            "repository_default" => Ok(Self::RepositoryDefault),
            other => Err(format!("unknown verify profile source: {other}")),
        }
    }
}

/// The resolved canonical verify profile.
///
/// Encapsulates the verification source, profile identity, and task-specific
/// check requirements that the freshness evaluator enforces.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CanonicalVerifyProfile {
    /// Human-readable profile name (e.g. "ci-default", "strict-local").
    pub profile_name: String,
    /// Where verification runs (CI, local, worker).
    pub verify_source: VerifySource,
    /// Optional command version constraint.
    pub command_version: Option<String>,
    /// Optional profile version identifier.
    pub profile_version: Option<String>,
    /// Checks that must appear in verify run coverage for this task
    /// (e.g. `["lint", "test", "typecheck"]`). Empty means no
    /// task-specific coverage requirement.
    pub required_checks: Vec<String>,
    /// Where this profile was resolved from (for audit metadata).
    pub source: VerifyProfileSource,
}

// ─── Freshness evaluation types ──────────────────────────────────────────────

/// Status of a file at evaluation time.
///
/// Captures the path and last-known modification timestamp (ISO-8601) so the
/// freshness evaluator can compare against the verify `completed_at` time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileStatus {
    /// Relative path within the repository.
    pub path: String,
    /// ISO-8601 timestamp of the file's last modification.
    pub modified_at: String,
}

/// Verdict of the freshness evaluation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FreshnessVerdict {
    /// `true` when the canonical verify is fresh and covers the current diff.
    pub fresh: bool,
    /// When `fresh` is `false`, the reason for rejection.
    pub reason: Option<FreshnessRejectionReason>,
}

impl FreshnessVerdict {
    /// A fresh verdict with no rejection reason.
    pub fn accept() -> Self {
        Self {
            fresh: true,
            reason: None,
        }
    }

    /// A stale verdict with the given rejection reason.
    pub fn reject(reason: FreshnessRejectionReason) -> Self {
        Self {
            fresh: false,
            reason: Some(reason),
        }
    }
}

/// Reasons a canonical verify freshness check can fail.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessRejectionReason {
    /// No canonical verify run exists for this task run.
    NoCanonicalVerify,
    /// Canonical verify exists but result was not `pass`.
    VerifyNotPass,
    /// The diff fingerprint does not match the current worker diff.
    DiffMismatch,
    /// A tracked or allowed-untracked file was modified after the verify
    /// completed (the verify is stale).
    ///
    /// Contains the path of the first file found to be stale.
    FileChangedAfterVerify(String),
    /// The task requires specific checks that are not present in the verify
    /// run's check coverage.
    ///
    /// Contains the names of the missing checks.
    MissingTaskChecks(Vec<String>),
}

// ─── Profile resolution ─────────────────────────────────────────────────────

/// Resolve the authoritative canonical verify profile using documented
/// precedence:
///
/// 1. **Task metadata override** — if the task's metadata specifies a verify
///    profile, it wins unconditionally.
/// 2. **Project verify configuration** — if the project has a verify config,
///    it is used when no task override exists.
/// 3. **Repository default profile** — the fallback when neither task nor
///    project specifies a profile.
///
/// Returns `None` when no profile can be resolved from any source (the caller
/// should treat this as "no canonical verify configured" and block auto-submit
/// with `FreshnessRejectionReason::NoCanonicalVerify`).
///
/// # Arguments
///
/// * `task_metadata_override` — optional profile name from task metadata
/// * `task_required_checks` — task-specific required checks from task metadata
///   (only used when a profile is resolved; not a substitute for a profile)
/// * `project_verify_config` — optional profile name from project configuration
/// * `project_required_checks` — project-level required checks
/// * `repo_default_profile` — optional default profile name from repository
///   configuration
/// * `repo_default_required_checks` — repo-level required checks
pub fn resolve_canonical_verify_profile(
    task_metadata_override: Option<&str>,
    task_required_checks: &[String],
    project_verify_config: Option<&str>,
    project_required_checks: &[String],
    repo_default_profile: Option<&str>,
    repo_default_required_checks: &[String],
) -> Option<CanonicalVerifyProfile> {
    // Precedence 1: Task metadata override.
    if let Some(profile) = non_empty(task_metadata_override) {
        return Some(build_profile(
            profile,
            task_required_checks,
            VerifyProfileSource::TaskMetadata,
        ));
    }

    // Precedence 2: Project verify configuration.
    if let Some(profile) = non_empty(project_verify_config) {
        return Some(build_profile(
            profile,
            project_required_checks,
            VerifyProfileSource::ProjectConfig,
        ));
    }

    // Precedence 3: Repository default profile.
    if let Some(profile) = non_empty(repo_default_profile) {
        return Some(build_profile(
            profile,
            repo_default_required_checks,
            VerifyProfileSource::RepositoryDefault,
        ));
    }

    None
}

// ─── Freshness evaluation ───────────────────────────────────────────────────

/// Evaluate whether the latest canonical verify run is fresh for the current
/// exact diff.
///
/// Checks, in order:
///
/// 1. A canonical verify run exists.
/// 2. The verify run result is `pass`.
/// 3. The verify run's `diff_fingerprint` matches the current diff fingerprint.
/// 4. No tracked or allowed-untracked file has been modified after the verify
///    run's `completed_at` timestamp.
/// 5. All task-specific required checks are present in the verify run's
///    check coverage.
///
/// Returns [`FreshnessVerdict::accept()`] when all checks pass, or
/// [`FreshnessVerdict::reject(reason)`] for the first failing check.
pub fn evaluate_freshness(
    current_diff_fingerprint: &str,
    verify_run: Option<&VerifyRunRecord>,
    tracked_files: &[FileStatus],
    allowed_untracked_files: &[FileStatus],
    required_checks: &[String],
) -> FreshnessVerdict {
    // 1. Must have a verify run.
    let run = match verify_run {
        Some(r) => r,
        None => return FreshnessVerdict::reject(FreshnessRejectionReason::NoCanonicalVerify),
    };

    // 2. Result must be `pass`.
    match run.result.parse::<VerifyResult>() {
        Ok(VerifyResult::Pass) => {}
        _ => return FreshnessVerdict::reject(FreshnessRejectionReason::VerifyNotPass),
    }

    // 3. Diff fingerprint must match exactly.
    if run.diff_fingerprint != current_diff_fingerprint {
        return FreshnessVerdict::reject(FreshnessRejectionReason::DiffMismatch);
    }

    // 4. No tracked or allowed-untracked file may have changed after verify.
    let completed_at = &run.completed_at;
    let all_files = tracked_files.iter().chain(allowed_untracked_files.iter());
    for file in all_files {
        if file.modified_at.as_str() > completed_at.as_str() {
            return FreshnessVerdict::reject(FreshnessRejectionReason::FileChangedAfterVerify(
                file.path.clone(),
            ));
        }
    }

    // 5. All task-specific required checks must be present in coverage.
    if !required_checks.is_empty() {
        let coverage = match &run.check_coverage {
            Some(serde_json::Value::Object(map)) => map,
            _ => {
                // No coverage at all → all required checks are missing.
                return FreshnessVerdict::reject(FreshnessRejectionReason::MissingTaskChecks(
                    required_checks.to_vec(),
                ));
            }
        };

        let missing: Vec<String> = required_checks
            .iter()
            .filter(|check| {
                !coverage
                    .get(check.as_str())
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            })
            .cloned()
            .collect();

        if !missing.is_empty() {
            return FreshnessVerdict::reject(FreshnessRejectionReason::MissingTaskChecks(missing));
        }
    }

    FreshnessVerdict::accept()
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Return `Some(s)` when the trimmed string is non-empty.
fn non_empty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

/// Build a `CanonicalVerifyProfile` with the given source, defaulting the
/// verify source to `Ci` for named profiles. Profiles starting with `"local"`
/// default to `Local`; profiles starting with `"worker"` default to `Worker`.
fn build_profile(
    profile_name: &str,
    required_checks: &[String],
    source: VerifyProfileSource,
) -> CanonicalVerifyProfile {
    let verify_source = if profile_name.starts_with("local") {
        VerifySource::Local
    } else if profile_name.starts_with("worker") {
        VerifySource::Worker
    } else {
        VerifySource::Ci
    };

    CanonicalVerifyProfile {
        profile_name: profile_name.to_owned(),
        verify_source,
        command_version: None,
        profile_version: None,
        required_checks: required_checks.to_vec(),
        source,
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Profile resolution tests ─────────────────────────────────────────────

    #[test]
    fn task_metadata_takes_highest_precedence() {
        let profile = resolve_canonical_verify_profile(
            Some("strict-ci"),
            &["lint".into(), "test".into()],
            Some("project-default"),
            &[],
            Some("repo-default"),
            &[],
        )
        .expect("must resolve");

        assert_eq!(profile.profile_name, "strict-ci");
        assert_eq!(profile.source, VerifyProfileSource::TaskMetadata);
        assert_eq!(profile.required_checks, vec!["lint", "test"]);
    }

    #[test]
    fn project_config_used_when_no_task_override() {
        let profile = resolve_canonical_verify_profile(
            None,
            &[],
            Some("project-ci"),
            &["typecheck".into()],
            Some("repo-default"),
            &[],
        )
        .expect("must resolve");

        assert_eq!(profile.profile_name, "project-ci");
        assert_eq!(profile.source, VerifyProfileSource::ProjectConfig);
        assert_eq!(profile.required_checks, vec!["typecheck"]);
    }

    #[test]
    fn repo_default_used_when_nothing_else() {
        let profile = resolve_canonical_verify_profile(
            None,
            &[],
            None,
            &[],
            Some("repo-default"),
            &["lint".into()],
        )
        .expect("must resolve");

        assert_eq!(profile.profile_name, "repo-default");
        assert_eq!(profile.source, VerifyProfileSource::RepositoryDefault);
        assert_eq!(profile.required_checks, vec!["lint"]);
    }

    #[test]
    fn returns_none_when_no_sources() {
        let result = resolve_canonical_verify_profile(None, &[], None, &[], None, &[]);
        assert!(result.is_none());
    }

    #[test]
    fn empty_strings_treated_as_absent() {
        let result = resolve_canonical_verify_profile(Some("  "), &[], Some(""), &[], None, &[]);
        assert!(result.is_none());
    }

    #[test]
    fn verify_source_derived_from_profile_name() {
        let ci = resolve_canonical_verify_profile(Some("ci-default"), &[], None, &[], None, &[])
            .unwrap();
        assert_eq!(ci.verify_source, VerifySource::Ci);

        let local =
            resolve_canonical_verify_profile(Some("local-check"), &[], None, &[], None, &[])
                .unwrap();
        assert_eq!(local.verify_source, VerifySource::Local);

        let worker =
            resolve_canonical_verify_profile(Some("worker-auto"), &[], None, &[], None, &[])
                .unwrap();
        assert_eq!(worker.verify_source, VerifySource::Worker);
    }

    // ── Freshness evaluation tests ───────────────────────────────────────────

    fn make_run(
        result: &str,
        diff_fingerprint: &str,
        completed_at: &str,
        check_coverage: Option<serde_json::Value>,
    ) -> VerifyRunRecord {
        VerifyRunRecord {
            id: "vr-1".to_owned(),
            task_run_id: "tr-1".to_owned(),
            verify_source: "ci".to_owned(),
            verify_run_id: "external-1".to_owned(),
            command_version: Some("1.0.0".to_owned()),
            profile_version: Some("v1".to_owned()),
            completed_at: completed_at.to_owned(),
            result: result.to_owned(),
            diff_fingerprint: diff_fingerprint.to_owned(),
            check_coverage,
            source_phase: None,
            verification_attempt_id: None,
            ordered_commands: None,
            covered_checks: None,
            verification_input_fingerprint: None,
            manifest_version: None,
            environment_identity_json: None,
            environment_identity_digest: None,
            environment_identity_version: None,
            created_at: "2025-01-15T10:30:00.000Z".to_owned(),
        }
    }

    #[test]
    fn fresh_exact_diff_accepted() {
        let run = make_run(
            "pass",
            "abc123",
            "2025-01-15T10:30:00.000Z",
            Some(serde_json::json!({"lint": true, "test": true})),
        );
        let verdict = evaluate_freshness(
            "abc123",
            Some(&run),
            &[],
            &[],
            &["lint".into(), "test".into()],
        );
        assert_eq!(verdict, FreshnessVerdict::accept());
    }

    #[test]
    fn stale_diff_rejected() {
        let run = make_run("pass", "abc123", "2025-01-15T10:30:00.000Z", None);
        let verdict = evaluate_freshness("different_fingerprint", Some(&run), &[], &[], &[]);
        assert_eq!(
            verdict,
            FreshnessVerdict::reject(FreshnessRejectionReason::DiffMismatch)
        );
    }

    #[test]
    fn missing_canonical_verify_rejected() {
        let verdict = evaluate_freshness("abc123", None, &[], &[], &[]);
        assert_eq!(
            verdict,
            FreshnessVerdict::reject(FreshnessRejectionReason::NoCanonicalVerify)
        );
    }

    #[test]
    fn failed_verify_rejected() {
        let run = make_run("fail", "abc123", "2025-01-15T10:30:00.000Z", None);
        let verdict = evaluate_freshness("abc123", Some(&run), &[], &[], &[]);
        assert_eq!(
            verdict,
            FreshnessVerdict::reject(FreshnessRejectionReason::VerifyNotPass)
        );
    }

    #[test]
    fn errored_verify_rejected() {
        let run = make_run("error", "abc123", "2025-01-15T10:30:00.000Z", None);
        let verdict = evaluate_freshness("abc123", Some(&run), &[], &[], &[]);
        assert_eq!(
            verdict,
            FreshnessVerdict::reject(FreshnessRejectionReason::VerifyNotPass)
        );
    }

    #[test]
    fn changed_after_verify_tracked_file_rejected() {
        let run = make_run("pass", "abc123", "2025-01-15T10:30:00.000Z", None);
        let tracked = vec![FileStatus {
            path: "src/main.rs".to_owned(),
            modified_at: "2025-01-15T11:00:00.000Z".to_owned(),
        }];
        let verdict = evaluate_freshness("abc123", Some(&run), &tracked, &[], &[]);
        assert_eq!(
            verdict,
            FreshnessVerdict::reject(FreshnessRejectionReason::FileChangedAfterVerify(
                "src/main.rs".to_owned()
            ))
        );
    }

    #[test]
    fn changed_after_verify_untracked_file_rejected() {
        let run = make_run("pass", "abc123", "2025-01-15T10:30:00.000Z", None);
        let untracked = vec![FileStatus {
            path: "generated/output.rs".to_owned(),
            modified_at: "2025-01-15T12:00:00.000Z".to_owned(),
        }];
        let verdict = evaluate_freshness("abc123", Some(&run), &[], &untracked, &[]);
        assert_eq!(
            verdict,
            FreshnessVerdict::reject(FreshnessRejectionReason::FileChangedAfterVerify(
                "generated/output.rs".to_owned()
            ))
        );
    }

    #[test]
    fn file_unchanged_after_verify_accepted() {
        let run = make_run("pass", "abc123", "2025-01-15T10:30:00.000Z", None);
        let tracked = vec![
            FileStatus {
                path: "src/main.rs".to_owned(),
                modified_at: "2025-01-15T10:00:00.000Z".to_owned(),
            },
            FileStatus {
                path: "src/lib.rs".to_owned(),
                modified_at: "2025-01-15T10:30:00.000Z".to_owned(), // equal is OK
            },
        ];
        let verdict = evaluate_freshness("abc123", Some(&run), &tracked, &[], &[]);
        assert_eq!(verdict, FreshnessVerdict::accept());
    }

    #[test]
    fn missing_task_checks_rejected() {
        let run = make_run(
            "pass",
            "abc123",
            "2025-01-15T10:30:00.000Z",
            Some(serde_json::json!({"lint": true})),
        );
        let verdict = evaluate_freshness(
            "abc123",
            Some(&run),
            &[],
            &[],
            &["lint".into(), "test".into()],
        );
        assert_eq!(
            verdict,
            FreshnessVerdict::reject(FreshnessRejectionReason::MissingTaskChecks(vec![
                "test".to_owned()
            ]))
        );
    }

    #[test]
    fn missing_task_checks_coverage_false_rejected() {
        let run = make_run(
            "pass",
            "abc123",
            "2025-01-15T10:30:00.000Z",
            Some(serde_json::json!({"lint": true, "test": false})),
        );
        let verdict = evaluate_freshness(
            "abc123",
            Some(&run),
            &[],
            &[],
            &["lint".into(), "test".into()],
        );
        assert_eq!(
            verdict,
            FreshnessVerdict::reject(FreshnessRejectionReason::MissingTaskChecks(vec![
                "test".to_owned()
            ]))
        );
    }

    #[test]
    fn no_check_coverage_object_rejected_when_checks_required() {
        let run = make_run(
            "pass",
            "abc123",
            "2025-01-15T10:30:00.000Z",
            Some(serde_json::json!("not-an-object")),
        );
        let verdict = evaluate_freshness(
            "abc123",
            Some(&run),
            &[],
            &[],
            &["lint".into(), "test".into()],
        );
        assert_eq!(
            verdict,
            FreshnessVerdict::reject(FreshnessRejectionReason::MissingTaskChecks(vec![
                "lint".to_owned(),
                "test".to_owned()
            ]))
        );
    }

    #[test]
    fn null_check_coverage_rejected_when_checks_required() {
        let run = make_run("pass", "abc123", "2025-01-15T10:30:00.000Z", None);
        let verdict = evaluate_freshness("abc123", Some(&run), &[], &[], &["lint".into()]);
        assert_eq!(
            verdict,
            FreshnessVerdict::reject(FreshnessRejectionReason::MissingTaskChecks(vec![
                "lint".to_owned()
            ]))
        );
    }

    #[test]
    fn all_checks_present_accepted() {
        let run = make_run(
            "pass",
            "abc123",
            "2025-01-15T10:30:00.000Z",
            Some(serde_json::json!({"lint": true, "test": true, "typecheck": true})),
        );
        let verdict = evaluate_freshness(
            "abc123",
            Some(&run),
            &[],
            &[],
            &["lint".into(), "test".into(), "typecheck".into()],
        );
        assert_eq!(verdict, FreshnessVerdict::accept());
    }

    #[test]
    fn empty_required_checks_means_no_coverage_needed() {
        let run = make_run("pass", "abc123", "2025-01-15T10:30:00.000Z", None);
        let verdict = evaluate_freshness("abc123", Some(&run), &[], &[], &[]);
        assert_eq!(verdict, FreshnessVerdict::accept());
    }

    // ── VerifyProfileSource parsing ──────────────────────────────────────────

    #[test]
    fn verify_profile_source_roundtrips() {
        for source in [
            VerifyProfileSource::TaskMetadata,
            VerifyProfileSource::ProjectConfig,
            VerifyProfileSource::RepositoryDefault,
        ] {
            let s = source.to_string();
            let parsed: VerifyProfileSource = s.parse().unwrap();
            assert_eq!(parsed, source);
        }
    }

    #[test]
    fn verify_profile_source_display() {
        assert_eq!(VerifyProfileSource::TaskMetadata.as_str(), "task_metadata");
        assert_eq!(
            VerifyProfileSource::ProjectConfig.as_str(),
            "project_config"
        );
        assert_eq!(
            VerifyProfileSource::RepositoryDefault.as_str(),
            "repository_default"
        );
    }

    // ── Serialization round-trips ────────────────────────────────────────────

    #[test]
    fn freshness_verdict_serializes_and_deserializes() {
        let accept = FreshnessVerdict::accept();
        let json = serde_json::to_string(&accept).unwrap();
        let back: FreshnessVerdict = serde_json::from_str(&json).unwrap();
        assert!(back.fresh);
        assert!(back.reason.is_none());

        let reject = FreshnessVerdict::reject(FreshnessRejectionReason::DiffMismatch);
        let json = serde_json::to_string(&reject).unwrap();
        let back: FreshnessVerdict = serde_json::from_str(&json).unwrap();
        assert!(!back.fresh);
        assert_eq!(back.reason.unwrap(), FreshnessRejectionReason::DiffMismatch);
    }

    #[test]
    fn canonical_verify_profile_serializes() {
        let profile = CanonicalVerifyProfile {
            profile_name: "test-profile".to_owned(),
            verify_source: VerifySource::Ci,
            command_version: Some("1.0".to_owned()),
            profile_version: Some("v2".to_owned()),
            required_checks: vec!["lint".into()],
            source: VerifyProfileSource::TaskMetadata,
        };
        let json = serde_json::to_string(&profile).unwrap();
        let back: CanonicalVerifyProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.profile_name, "test-profile");
        assert_eq!(back.source, VerifyProfileSource::TaskMetadata);
    }
}
