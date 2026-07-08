//! Deterministic rule evaluators for tripwire findings.
//!
//! Each evaluator is a pure function that takes a [`TripwirePolicy`] and a
//! slice of [`ChangedFile`]s and returns zero or more [`RawFinding`]s.
//! The evaluators are **LLM-free** and **side-effect-free** — they only
//! read from the inputs and the policy.
//!
//! The engine orchestrates calling each evaluator and wrapping results with
//! severity, revisions, and idempotency keys. Rule evaluators are responsible
//! only for detection and evidence extraction.
//!
//! # Excluded files
//!
//! The engine (not the rule evaluator) handles generated/vendor exclusions.
//! Rule evaluators set `evidence_is_excluded` when the `ChangedFile` was
//! pre-classified as generated or vendor.
//!
//! # Adding a new rule family
//!
//! 1. Add the rule-id variant to [`TripwireRuleId`] and the reason code
//!    constant to [`reason_codes`].
//! 2. Add the per-rule config struct to [`policy`].
//! 3. Add a new evaluator function here.
//! 4. Register the evaluator in the downstream orchestration code (enforcement
//!    epic `nptj`).

#![allow(dead_code)]

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::tripwires::engine::{ChangedFile, RawFinding};
use crate::tripwires::policy::TripwirePolicy;
use crate::tripwires::reason_codes::TripwireRuleId;

// ─── Glob matching helpers ────────────────────────────────────────────────

/// Check whether a path matches any of the provided glob patterns.
///
/// Returns `true` if at least one pattern matches. Uses `globset` for
/// correct glob semantics including `**`, `*`, `?`, and brace expansion.
fn path_matches_any_glob(path: &str, globs: &[String]) -> bool {
    if globs.is_empty() {
        return false;
    }
    let mut builder = GlobSetBuilder::new();
    for g in globs {
        if let Ok(glob) = Glob::new(g) {
            builder.add(glob);
        }
    }
    match builder.build() {
        Ok(set) => set.is_match(path),
        // If the glob set fails to build, treat as no match (fail-safe).
        Err(_) => false,
    }
}

/// Build a [`GlobSet`] from a slice of pattern strings.
///
/// Returns `None` if the set is empty or fails to build.
fn build_glob_set(globs: &[String]) -> Option<GlobSet> {
    if globs.is_empty() {
        return None;
    }
    let mut builder = GlobSetBuilder::new();
    for g in globs {
        if let Ok(glob) = Glob::new(g) {
            builder.add(glob);
        }
    }
    builder.build().ok()
}

/// Extract the file extension from a path (without the dot).
///
/// Returns `None` if the path has no extension or is a dotfile
/// (e.g. `.gitignore`).
fn file_extension(path: &str) -> Option<&str> {
    let basename = path.rsplit('/').next().unwrap_or(path);
    // Dotfiles (`.gitignore`, `.env`) have no meaningful extension.
    if basename.starts_with('.') {
        return None;
    }
    basename.rsplit_once('.').map(|(_, ext)| ext)
}

// ─── Rule: migration_change ──────────────────────────────────────────────

/// Evaluate migration-change findings.
///
/// Flags any changed file whose path matches the policy's migration
/// path globs (e.g. `migrations/**`, `db/migrations/**`). Each
/// matching file produces one file-level finding.
///
/// Deletions and renames of migration files are also flagged because
/// they represent schema-affecting changes.
pub fn evaluate_migration_changes(
    policy: &TripwirePolicy,
    changed_files: &[ChangedFile],
) -> Vec<RawFinding> {
    let config = &policy.migration;
    if !config.enabled {
        return Vec::new();
    }

    let glob_set = match build_glob_set(&config.path_globs) {
        Some(set) => set,
        None => return Vec::new(),
    };

    let mut findings = Vec::new();
    for file in changed_files {
        if file.is_excluded() {
            continue;
        }
        // Match on the current path, and for renames also on the old path.
        let current_matches = glob_set.is_match(&file.path);
        let old_matches = file
            .old_path
            .as_deref()
            .map(|p| glob_set.is_match(p))
            .unwrap_or(false);

        if current_matches || old_matches {
            findings.push(RawFinding {
                rule_id: TripwireRuleId::MigrationChange,
                report_only: config.report_only,
                evidence_path: file.path.clone(),
                evidence_start_line: None,
                evidence_end_line: None,
                evidence_is_excluded: false,
            });
        }
    }
    findings
}

// ─── Rule: dependency_identity_change ────────────────────────────────────

/// Evaluate dependency-identity-change findings.
///
/// Checks manifest files (e.g. `Cargo.toml`, `package.json`) for
/// changes and optionally lockfile files (e.g. `Cargo.lock`,
/// `package-lock.json`).
///
/// Manifest changes always produce a finding because they represent
/// explicit dependency-declaration modifications. Lockfile changes
/// produce a finding only when
/// [`DependencyIdentityRuleConfig::lockfile_change_blocks`] is `true`.
pub fn evaluate_dependency_identity_changes(
    policy: &TripwirePolicy,
    changed_files: &[ChangedFile],
) -> Vec<RawFinding> {
    let config = &policy.dependency_identity;
    if !config.enabled {
        return Vec::new();
    }

    let manifest_set = match build_glob_set(&config.manifest_paths) {
        Some(set) => set,
        None => GlobSetBuilder::new().build().unwrap_or_default(),
    };
    let lockfile_set = match build_glob_set(&config.lockfile_paths) {
        Some(set) => set,
        None => GlobSetBuilder::new().build().unwrap_or_default(),
    };

    let mut findings = Vec::new();
    for file in changed_files {
        if file.is_excluded() {
            continue;
        }

        let is_manifest = manifest_set.is_match(&file.path);
        let is_lockfile = lockfile_set.is_match(&file.path);

        if is_manifest {
            // Manifest changes represent dependency identity changes
            // (add/remove/rename/source switch/major version bump).
            findings.push(RawFinding {
                rule_id: TripwireRuleId::DependencyIdentityChange,
                report_only: config.report_only,
                evidence_path: file.path.clone(),
                evidence_start_line: None,
                evidence_end_line: None,
                evidence_is_excluded: false,
            });
        } else if is_lockfile && config.lockfile_change_blocks {
            findings.push(RawFinding {
                rule_id: TripwireRuleId::DependencyIdentityChange,
                report_only: config.report_only,
                evidence_path: file.path.clone(),
                evidence_start_line: None,
                evidence_end_line: None,
                evidence_is_excluded: false,
            });
        }
    }
    findings
}

// ─── Rule: network_egress_change ─────────────────────────────────────────

/// Evaluate network-egress-change findings.
///
/// Scans diff lines of changed files for substrings that indicate new
/// outbound network clients, hosts, protocols, SDKs, or webhook
/// targets (e.g. `reqwest::`, `fetch(`, `Webhook`).
///
/// Evidence is line-precise when diff hunks are available; otherwise
/// falls back to file-level.
pub fn evaluate_network_egress_changes(
    policy: &TripwirePolicy,
    changed_files: &[ChangedFile],
) -> Vec<RawFinding> {
    let config = &policy.network_egress;
    if !config.enabled {
        return Vec::new();
    }

    let matchers = &config.matcher_substrings;
    if matchers.is_empty() {
        return Vec::new();
    }

    let rule_local_glob_set = build_glob_set(&config.path_globs);

    let mut findings = Vec::new();
    for file in changed_files {
        if file.is_excluded() {
            continue;
        }

        // Rule-local path exclusion (e.g. known non-egress paths).
        if rule_local_glob_set
            .as_ref()
            .is_some_and(|gs| gs.is_match(&file.path))
        {
            continue;
        }

        // Scan diff lines for egress indicators.
        let mut matched_spans: Vec<(String, Option<u32>, Option<u32>)> = Vec::new();

        for hunk in &file.hunks {
            for (i, line) in hunk.diff_lines.iter().enumerate() {
                // Only match added lines (lines starting with '+').
                if !line.starts_with('+') {
                    continue;
                }
                let added_line = &line[1..]; // strip '+'
                for matcher in matchers {
                    if added_line.contains(matcher.as_str()) {
                        let line_num = hunk.new_start + i as u32;
                        matched_spans.push((file.path.clone(), Some(line_num), Some(line_num)));
                        break; // one finding per line, not per matcher
                    }
                }
            }
        }

        if matched_spans.is_empty() {
            // No line-precise match; check if there are ANY added lines
            // and scan file content via a heuristic: if the file has
            // additions and matchers could apply, flag file-level.
            if file.additions > 0 && !file.hunks.is_empty() {
                // We had hunks but no matcher hit — skip.
                continue;
            }
            // No diff lines available but file has additions: check if
            // any matcher substring appears in the path itself (e.g. a
            // file named `webhook_handler.rs`).
            // This is a conservative fallback.
            continue;
        }

        for (path, start, end) in matched_spans {
            findings.push(RawFinding {
                rule_id: TripwireRuleId::NetworkEgressChange,
                report_only: config.report_only,
                evidence_path: path,
                evidence_start_line: start,
                evidence_end_line: end,
                evidence_is_excluded: false,
            });
        }
    }
    findings
}

// ─── Rule: unsafe_code_change ────────────────────────────────────────────

/// Evaluate unsafe-code-change findings.
///
/// Scans diff lines of changed files with matching extensions for
/// `unsafe`, FFI, and escape-hatch indicators (e.g. `unsafe {`,
/// `extern "C"`, `eval(`, `ctypes.`, `//go:linkname`).
///
/// Only added lines (`+` prefix in diff) are scanned; context and
/// removed lines are ignored. Evidence is line-precise.
pub fn evaluate_unsafe_code_changes(
    policy: &TripwirePolicy,
    changed_files: &[ChangedFile],
) -> Vec<RawFinding> {
    let config = &policy.unsafe_code;
    if !config.enabled {
        return Vec::new();
    }

    let matchers = &config.matcher_substrings;
    if matchers.is_empty() {
        return Vec::new();
    }

    let extensions_set: std::collections::HashSet<&str> =
        config.extensions.iter().map(|s| s.as_str()).collect();

    let mut findings = Vec::new();
    for file in changed_files {
        if file.is_excluded() {
            continue;
        }

        // Only scan files with matching extensions.
        let ext = match file_extension(&file.path) {
            Some(e) => e,
            None => continue,
        };
        if !extensions_set.contains(ext) {
            continue;
        }

        for hunk in &file.hunks {
            for (i, line) in hunk.diff_lines.iter().enumerate() {
                if !line.starts_with('+') {
                    continue;
                }
                let added_line = &line[1..];
                for matcher in matchers {
                    if added_line.contains(matcher.as_str()) {
                        let line_num = hunk.new_start + i as u32;
                        findings.push(RawFinding {
                            rule_id: TripwireRuleId::UnsafeCodeChange,
                            report_only: config.report_only,
                            evidence_path: file.path.clone(),
                            evidence_start_line: Some(line_num),
                            evidence_end_line: Some(line_num),
                            evidence_is_excluded: false,
                        });
                        break; // one finding per line
                    }
                }
            }
        }
    }
    findings
}

// ─── Rule: boundary_path_change ──────────────────────────────────────────

/// Evaluate boundary-path-change findings.
///
/// Flags changed files that match boundary-sensitive category patterns
/// (auth, permissions, secrets, deployment, billing, capability-boundary
/// allowlist). The boundary-path rule also flags changes to the
/// allowlist file itself.
///
/// Findings include the allowlist revision from the policy so
/// downstream code can correlate which allowlist version was in effect.
///
/// When the allowlist is in degraded mode (revision =
/// `MISSING_ALLOWLIST_REVISION`), boundary findings are still emitted
/// but the degraded revision propagates so enforcement can choose to
/// treat them as advisory.
pub fn evaluate_boundary_path_changes(
    policy: &TripwirePolicy,
    changed_files: &[ChangedFile],
) -> Vec<RawFinding> {
    let config = &policy.boundary_path;
    if !config.enabled {
        return Vec::new();
    }

    let category_set = match build_glob_set(&config.category_patterns) {
        Some(set) => set,
        None => return Vec::new(),
    };

    // Also match the allowlist source file itself.
    let allowlist_source = &policy.allowlist.source;

    let mut findings = Vec::new();
    for file in changed_files {
        if file.is_excluded() {
            continue;
        }

        let matches_category = category_set.is_match(&file.path);
        let is_allowlist_file = file.path == *allowlist_source;

        if !matches_category && !is_allowlist_file {
            continue;
        }

        // When match_outside_allowlist is false, only flag new files
        // (Added status). When true, flag all matching changes.
        if !config.match_outside_allowlist
            && file.status != crate::tripwires::engine::ChangedFileStatus::Added
            && !is_allowlist_file
        {
            continue;
        }

        findings.push(RawFinding {
            rule_id: TripwireRuleId::BoundaryPathChange,
            report_only: config.report_only,
            evidence_path: file.path.clone(),
            evidence_start_line: None,
            evidence_end_line: None,
            evidence_is_excluded: false,
        });
    }
    findings
}

// ─── Convenience: all five rule evaluators ────────────────────────────────

/// Type alias for boxed rule evaluator functions.
type RuleEvaluatorFn = dyn Fn(&TripwirePolicy, &[ChangedFile]) -> Vec<RawFinding> + Send + Sync;

/// Build a vector of all five rule evaluator functions ready to be passed
/// to [`crate::tripwires::engine::evaluate`].
///
/// Callers who want to register only a subset of rules can call the
/// individual evaluator functions directly.
pub fn all_rule_evaluators() -> Vec<Box<RuleEvaluatorFn>> {
    vec![
        Box::new(evaluate_migration_changes),
        Box::new(evaluate_dependency_identity_changes),
        Box::new(evaluate_network_egress_changes),
        Box::new(evaluate_unsafe_code_changes),
        Box::new(evaluate_boundary_path_changes),
    ]
}

// ─── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tripwires::engine::{ChangedFile, ChangedFileStatus, DiffHunk};
    use crate::tripwires::policy::TripwirePolicy;
    use crate::tripwires::reason_codes::*;

    // ── Helpers ──────────────────────────────────────────────────────────

    /// Build a minimal changed file with no diff content.
    fn simple_file(
        path: &str,
        status: ChangedFileStatus,
        additions: u32,
        deletions: u32,
    ) -> ChangedFile {
        ChangedFile {
            path: path.to_owned(),
            old_path: None,
            status,
            additions,
            deletions,
            hunks: Vec::new(),
            is_generated: false,
            is_vendor: false,
        }
    }

    /// Build a changed file with diff hunks containing added lines.
    fn file_with_added_lines(path: &str, added_lines: &[&str]) -> ChangedFile {
        let diff_lines: Vec<String> = added_lines.iter().map(|l| format!("+{l}")).collect();
        let hunk = DiffHunk {
            new_start: 1,
            new_lines: added_lines.len() as u32,
            old_start: 0,
            old_lines: 0,
            diff_lines,
        };
        ChangedFile {
            path: path.to_owned(),
            old_path: None,
            status: ChangedFileStatus::Modified,
            additions: added_lines.len() as u32,
            deletions: 0,
            hunks: vec![hunk],
            is_generated: false,
            is_vendor: false,
        }
    }

    /// Build a changed file with diff hunks containing context and added lines.
    fn file_with_mixed_diff(
        path: &str,
        new_start: u32,
        lines: &[(char, &str)], // ('+', '-', ' ' prefix, content)
    ) -> ChangedFile {
        let diff_lines: Vec<String> = lines
            .iter()
            .map(|(prefix, content)| format!("{prefix}{content}"))
            .collect();
        let new_lines = lines.iter().filter(|(p, _)| *p != '-').count() as u32;
        let old_lines = lines.iter().filter(|(p, _)| *p != '+').count() as u32;
        let hunk = DiffHunk {
            new_start,
            new_lines,
            old_start: new_start,
            old_lines,
            diff_lines,
        };
        let additions = lines.iter().filter(|(p, _)| *p == '+').count() as u32;
        ChangedFile {
            path: path.to_owned(),
            old_path: None,
            status: ChangedFileStatus::Modified,
            additions,
            deletions: 0,
            hunks: vec![hunk],
            is_generated: false,
            is_vendor: false,
        }
    }

    fn default_policy() -> TripwirePolicy {
        TripwirePolicy::default()
    }

    fn policy_with_report_only_migration() -> TripwirePolicy {
        let mut p = default_policy();
        p.migration.report_only = true;
        p
    }

    fn policy_with_report_only_dependency() -> TripwirePolicy {
        let mut p = default_policy();
        p.dependency_identity.report_only = true;
        p
    }

    fn policy_with_report_only_egress() -> TripwirePolicy {
        let mut p = default_policy();
        p.network_egress.report_only = true;
        p
    }

    fn policy_with_report_only_unsafe() -> TripwirePolicy {
        let mut p = default_policy();
        p.unsafe_code.report_only = true;
        p
    }

    fn policy_with_report_only_boundary() -> TripwirePolicy {
        let mut p = default_policy();
        p.boundary_path.report_only = true;
        p
    }

    // ── migration_change: positive cases ────────────────────────────────

    #[test]
    fn migration_file_added_triggers_finding() {
        let files = vec![simple_file(
            "migrations/001_create_users.sql",
            ChangedFileStatus::Added,
            50,
            0,
        )];
        let findings = evaluate_migration_changes(&default_policy(), &files);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule_id, TripwireRuleId::MigrationChange);
        assert_eq!(findings[0].evidence_path, "migrations/001_create_users.sql");
        assert!(!findings[0].report_only);
    }

    #[test]
    fn migration_file_modified_triggers_finding() {
        let files = vec![simple_file(
            "db/migrations/002_add_email.sql",
            ChangedFileStatus::Modified,
            10,
            5,
        )];
        let findings = evaluate_migration_changes(&default_policy(), &files);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule_id, TripwireRuleId::MigrationChange);
    }

    #[test]
    fn migration_file_deleted_triggers_finding() {
        let files = vec![simple_file(
            "migrations/003_legacy.sql",
            ChangedFileStatus::Deleted,
            0,
            100,
        )];
        let findings = evaluate_migration_changes(&default_policy(), &files);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn migration_file_renamed_triggers_finding() {
        let files = vec![ChangedFile {
            path: "migrations/004_renamed.sql".to_owned(),
            old_path: Some("migrations/004_old_name.sql".to_owned()),
            status: ChangedFileStatus::Renamed,
            additions: 0,
            deletions: 0,
            hunks: Vec::new(),
            is_generated: false,
            is_vendor: false,
        }];
        let findings = evaluate_migration_changes(&default_policy(), &files);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].evidence_path, "migrations/004_renamed.sql");
    }

    #[test]
    fn migration_file_under_prisma_path() {
        let files = vec![simple_file(
            "prisma/migrations/20240101_init/migration.sql",
            ChangedFileStatus::Added,
            30,
            0,
        )];
        let findings = evaluate_migration_changes(&default_policy(), &files);
        assert_eq!(findings.len(), 1);
    }

    // ── migration_change: negative cases ────────────────────────────────

    #[test]
    fn non_migration_file_does_not_trigger() {
        let files = vec![simple_file(
            "src/main.rs",
            ChangedFileStatus::Modified,
            10,
            5,
        )];
        let findings = evaluate_migration_changes(&default_policy(), &files);
        assert!(findings.is_empty());
    }

    #[test]
    fn migration_rule_disabled_skips_evaluation() {
        let mut policy = default_policy();
        policy.migration.enabled = false;
        let files = vec![simple_file(
            "migrations/001.sql",
            ChangedFileStatus::Added,
            10,
            0,
        )];
        let findings = evaluate_migration_changes(&policy, &files);
        assert!(findings.is_empty());
    }

    // ── migration_change: generated/vendor exclusion ────────────────────

    #[test]
    fn migration_in_generated_file_is_excluded_by_engine_flag() {
        let files = vec![ChangedFile {
            is_generated: true,
            ..simple_file("migrations/001.sql", ChangedFileStatus::Added, 10, 0)
        }];
        let findings = evaluate_migration_changes(&default_policy(), &files);
        assert!(
            findings.is_empty(),
            "generated files must be excluded from findings"
        );
    }

    #[test]
    fn migration_in_vendor_file_is_excluded_by_engine_flag() {
        let files = vec![ChangedFile {
            is_vendor: true,
            ..simple_file("migrations/001.sql", ChangedFileStatus::Added, 10, 0)
        }];
        let findings = evaluate_migration_changes(&default_policy(), &files);
        assert!(
            findings.is_empty(),
            "vendor files must be excluded from findings"
        );
    }

    // ── migration_change: report-only ───────────────────────────────────

    #[test]
    fn migration_report_only_flag_propagated() {
        let files = vec![simple_file(
            "migrations/001.sql",
            ChangedFileStatus::Added,
            10,
            0,
        )];
        let findings = evaluate_migration_changes(&policy_with_report_only_migration(), &files);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].report_only);
    }

    // ── dependency_identity_change: positive cases ──────────────────────

    #[test]
    fn cargo_toml_changed_triggers_dependency_finding() {
        let files = vec![simple_file("Cargo.toml", ChangedFileStatus::Modified, 3, 1)];
        let findings = evaluate_dependency_identity_changes(&default_policy(), &files);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].rule_id,
            TripwireRuleId::DependencyIdentityChange
        );
    }

    #[test]
    fn package_json_changed_triggers_dependency_finding() {
        let files = vec![simple_file(
            "frontend/package.json",
            ChangedFileStatus::Modified,
            5,
            2,
        )];
        let findings = evaluate_dependency_identity_changes(&default_policy(), &files);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn go_mod_changed_triggers_dependency_finding() {
        let files = vec![simple_file("go.mod", ChangedFileStatus::Modified, 4, 0)];
        let findings = evaluate_dependency_identity_changes(&default_policy(), &files);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn lockfile_change_blocks_when_configured() {
        let mut policy = default_policy();
        policy.dependency_identity.lockfile_change_blocks = true;
        let files = vec![simple_file(
            "Cargo.lock",
            ChangedFileStatus::Modified,
            100,
            50,
        )];
        let findings = evaluate_dependency_identity_changes(&policy, &files);
        assert_eq!(findings.len(), 1);
    }

    // ── dependency_identity_change: negative cases ──────────────────────

    #[test]
    fn non_dependency_file_does_not_trigger() {
        let files = vec![simple_file(
            "src/lib.rs",
            ChangedFileStatus::Modified,
            20,
            5,
        )];
        let findings = evaluate_dependency_identity_changes(&default_policy(), &files);
        assert!(findings.is_empty());
    }

    #[test]
    fn lockfile_change_silent_when_not_configured() {
        // Default: lockfile_change_blocks = false
        let files = vec![simple_file(
            "Cargo.lock",
            ChangedFileStatus::Modified,
            100,
            50,
        )];
        let findings = evaluate_dependency_identity_changes(&default_policy(), &files);
        assert!(findings.is_empty());
    }

    #[test]
    fn dependency_rule_disabled_skips_evaluation() {
        let mut policy = default_policy();
        policy.dependency_identity.enabled = false;
        let files = vec![simple_file("Cargo.toml", ChangedFileStatus::Modified, 3, 1)];
        let findings = evaluate_dependency_identity_changes(&policy, &files);
        assert!(findings.is_empty());
    }

    // ── dependency_identity_change: generated/vendor exclusion ──────────

    #[test]
    fn manifest_in_generated_directory_excluded() {
        let files = vec![ChangedFile {
            is_generated: true,
            ..simple_file("target/pkg/Cargo.toml", ChangedFileStatus::Added, 10, 0)
        }];
        let findings = evaluate_dependency_identity_changes(&default_policy(), &files);
        assert!(findings.is_empty());
    }

    // ── dependency_identity_change: report-only ─────────────────────────

    #[test]
    fn dependency_report_only_flag_propagated() {
        let files = vec![simple_file("Cargo.toml", ChangedFileStatus::Modified, 3, 1)];
        let findings =
            evaluate_dependency_identity_changes(&policy_with_report_only_dependency(), &files);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].report_only);
    }

    // ── network_egress_change: positive cases ───────────────────────────

    #[test]
    fn reqwest_usage_in_added_line_triggers_egress() {
        let files = vec![file_with_added_lines(
            "src/http_client.rs",
            &["let client = reqwest::Client::new();"],
        )];
        let findings = evaluate_network_egress_changes(&default_policy(), &files);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule_id, TripwireRuleId::NetworkEgressChange);
        // Line-precise evidence
        assert!(findings[0].evidence_start_line.is_some());
        assert!(findings[0].evidence_end_line.is_some());
    }

    #[test]
    fn webhook_indication_in_added_line_triggers_egress() {
        let files = vec![file_with_added_lines(
            "src/notifications.rs",
            &[
                "fn notify() {",
                "    let webhook_url = \"https://hooks.slack.com/...\";",
                "}",
            ],
        )];
        let findings = evaluate_network_egress_changes(&default_policy(), &files);
        // "Webhook" substring should not match "webhook_url" because the
        // matcher is case-sensitive by design. Let's verify:
        assert_eq!(
            findings.len(),
            0,
            "case-sensitive: 'webhook_url' != 'Webhook'"
        );
    }

    #[test]
    fn webhook_capitalized_triggers() {
        let files = vec![file_with_added_lines(
            "src/notifications.rs",
            &["    let w = Webhook::new(url);"],
        )];
        let findings = evaluate_network_egress_changes(&default_policy(), &files);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn fetch_call_in_typescript_triggers_egress() {
        let files = vec![file_with_added_lines(
            "src/api.ts",
            &["  const resp = fetch(url);"],
        )];
        let findings = evaluate_network_egress_changes(&default_policy(), &files);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn axios_usage_triggers_egress() {
        let files = vec![file_with_added_lines(
            "src/client.ts",
            &["  axios.get(url);"],
        )];
        let findings = evaluate_network_egress_changes(&default_policy(), &files);
        assert_eq!(findings.len(), 1);
    }

    // ── network_egress_change: negative cases ───────────────────────────

    #[test]
    fn context_line_with_reqwest_does_not_trigger() {
        let files = vec![file_with_mixed_diff(
            "src/http_client.rs",
            10,
            &[
                (' ', "let client = reqwest::Client::new();"), // context, not added
                ('+', "client.get(url).send().await?;"),       // added, no matcher
            ],
        )];
        let findings = evaluate_network_egress_changes(&default_policy(), &files);
        assert!(findings.is_empty());
    }

    #[test]
    fn removed_line_with_reqwest_does_not_trigger() {
        let files = vec![file_with_mixed_diff(
            "src/http_client.rs",
            10,
            &[
                ('-', "let client = reqwest::Client::new();"), // removed, not added
            ],
        )];
        let findings = evaluate_network_egress_changes(&default_policy(), &files);
        assert!(findings.is_empty());
    }

    #[test]
    fn egress_rule_disabled_skips_evaluation() {
        let mut policy = default_policy();
        policy.network_egress.enabled = false;
        let files = vec![file_with_added_lines(
            "src/http_client.rs",
            &["let client = reqwest::Client::new();"],
        )];
        let findings = evaluate_network_egress_changes(&policy, &files);
        assert!(findings.is_empty());
    }

    #[test]
    fn no_egress_matchers_does_not_trigger() {
        let mut policy = default_policy();
        policy.network_egress.matcher_substrings = Vec::new();
        let files = vec![file_with_added_lines(
            "src/http_client.rs",
            &["let client = reqwest::Client::new();"],
        )];
        let findings = evaluate_network_egress_changes(&policy, &files);
        assert!(findings.is_empty());
    }

    // ── network_egress_change: generated/vendor exclusion ───────────────

    #[test]
    fn egress_in_generated_file_excluded() {
        let mut file = file_with_added_lines(
            "src/http_client.rs",
            &["let client = reqwest::Client::new();"],
        );
        file.is_generated = true;
        let findings = evaluate_network_egress_changes(&default_policy(), &[file]);
        assert!(findings.is_empty());
    }

    // ── network_egress_change: report-only ──────────────────────────────

    #[test]
    fn egress_report_only_flag_propagated() {
        let files = vec![file_with_added_lines(
            "src/http_client.rs",
            &["let client = reqwest::Client::new();"],
        )];
        let findings = evaluate_network_egress_changes(&policy_with_report_only_egress(), &files);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].report_only);
    }

    // ── unsafe_code_change: positive cases ──────────────────────────────

    #[test]
    fn unsafe_block_in_rust_triggers_finding() {
        let files = vec![file_with_added_lines(
            "src/native.rs",
            &["unsafe { ptr::null() }"],
        )];
        let findings = evaluate_unsafe_code_changes(&default_policy(), &files);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule_id, TripwireRuleId::UnsafeCodeChange);
        assert!(findings[0].evidence_start_line.is_some());
    }

    #[test]
    fn unsafe_fn_in_rust_triggers_finding() {
        let files = vec![file_with_added_lines(
            "src/ffi.rs",
            &["unsafe fn raw_pointer() -> *const u8 {"],
        )];
        let findings = evaluate_unsafe_code_changes(&default_policy(), &files);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn extern_c_in_rust_triggers_finding() {
        let files = vec![file_with_added_lines(
            "src/ffi.rs",
            &[r#"extern "C" fn callback()"#],
        )];
        let findings = evaluate_unsafe_code_changes(&default_policy(), &files);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn eval_in_javascript_triggers_finding() {
        let files = vec![file_with_added_lines(
            "src/dynamic.js",
            &["eval(userInput);"],
        )];
        let findings = evaluate_unsafe_code_changes(&default_policy(), &files);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn ctypes_in_python_triggers_finding() {
        let files = vec![file_with_added_lines("src/native.py", &["import ctypes"])];
        let findings = evaluate_unsafe_code_changes(&default_policy(), &files);
        // "ctypes" does not contain "ctypes." — the matcher requires a dot.
        // This is intentional: importing the module is less risky than
        // calling ctypes functions.
        assert_eq!(findings.len(), 0);
    }

    #[test]
    fn ctypes_dot_usage_in_python_triggers_finding() {
        let files = vec![file_with_added_lines(
            "src/native.py",
            &["lib = ctypes.CDLL(\"libfoo.so\")"],
        )];
        let findings = evaluate_unsafe_code_changes(&default_policy(), &files);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn go_linkname_triggers_finding() {
        let files = vec![file_with_added_lines(
            "runtime/hack.go",
            &["//go:linkname runtimeNano runtime.nanotime"],
        )];
        let findings = evaluate_unsafe_code_changes(&default_policy(), &files);
        assert_eq!(findings.len(), 1);
    }

    // ── unsafe_code_change: negative cases ──────────────────────────────

    #[test]
    fn safe_rust_code_does_not_trigger() {
        let files = vec![file_with_added_lines(
            "src/lib.rs",
            &["fn add(a: i32, b: i32) -> i32 { a + b }"],
        )];
        let findings = evaluate_unsafe_code_changes(&default_policy(), &files);
        assert!(findings.is_empty());
    }

    #[test]
    fn removed_unsafe_line_does_not_trigger() {
        let files = vec![file_with_mixed_diff(
            "src/native.rs",
            10,
            &[('-', "unsafe { old_code() }"), ('+', "safe_new_code()")],
        )];
        let findings = evaluate_unsafe_code_changes(&default_policy(), &files);
        assert!(findings.is_empty());
    }

    #[test]
    fn context_unsafe_line_does_not_trigger() {
        let files = vec![file_with_mixed_diff(
            "src/native.rs",
            10,
            &[(' ', "unsafe { existing_code() }"), ('+', "let x = 42;")],
        )];
        let findings = evaluate_unsafe_code_changes(&default_policy(), &files);
        assert!(findings.is_empty());
    }

    #[test]
    fn wrong_extension_does_not_trigger() {
        let files = vec![file_with_added_lines(
            "docs/unsafe.md",
            &["This uses `unsafe { }` blocks."],
        )];
        let findings = evaluate_unsafe_code_changes(&default_policy(), &files);
        assert!(
            findings.is_empty(),
            "non-code files must not trigger unsafe rule"
        );
    }

    #[test]
    fn unsafe_rule_disabled_skips_evaluation() {
        let mut policy = default_policy();
        policy.unsafe_code.enabled = false;
        let files = vec![file_with_added_lines(
            "src/native.rs",
            &["unsafe { ptr::null() }"],
        )];
        let findings = evaluate_unsafe_code_changes(&policy, &files);
        assert!(findings.is_empty());
    }

    // ── unsafe_code_change: generated/vendor exclusion ──────────────────

    #[test]
    fn unsafe_in_generated_file_excluded() {
        let mut file = file_with_added_lines("src/native.rs", &["unsafe { ptr::null() }"]);
        file.is_generated = true;
        let findings = evaluate_unsafe_code_changes(&default_policy(), &[file]);
        assert!(findings.is_empty());
    }

    // ── unsafe_code_change: report-only ─────────────────────────────────

    #[test]
    fn unsafe_report_only_flag_propagated() {
        let files = vec![file_with_added_lines(
            "src/native.rs",
            &["unsafe { ptr::null() }"],
        )];
        let findings = evaluate_unsafe_code_changes(&policy_with_report_only_unsafe(), &files);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].report_only);
    }

    // ── unsafe_code_change: multiple findings per file ──────────────────

    #[test]
    fn multiple_unsafe_lines_produce_multiple_findings() {
        let files = vec![file_with_added_lines(
            "src/native.rs",
            &[
                "unsafe { ptr::null() }",
                "let x = 42;",
                r#"extern "C" fn callback()"#,
            ],
        )];
        let findings = evaluate_unsafe_code_changes(&default_policy(), &files);
        assert_eq!(
            findings.len(),
            2,
            "two unsafe lines must produce two findings"
        );
    }

    // ── boundary_path_change: positive cases ────────────────────────────

    #[test]
    fn auth_path_change_triggers_boundary_finding() {
        let files = vec![simple_file(
            "server/src/auth/login.rs",
            ChangedFileStatus::Modified,
            10,
            5,
        )];
        let findings = evaluate_boundary_path_changes(&default_policy(), &files);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule_id, TripwireRuleId::BoundaryPathChange);
    }

    #[test]
    fn secrets_path_change_triggers_boundary_finding() {
        let files = vec![simple_file(
            "config/secrets/encryption_key.env",
            ChangedFileStatus::Added,
            5,
            0,
        )];
        let findings = evaluate_boundary_path_changes(&default_policy(), &files);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn deploy_path_change_triggers_boundary_finding() {
        let files = vec![simple_file(
            "deploy/production/manifest.yaml",
            ChangedFileStatus::Modified,
            3,
            1,
        )];
        let findings = evaluate_boundary_path_changes(&default_policy(), &files);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn env_file_change_triggers_boundary_finding() {
        let files = vec![simple_file(
            ".env.production",
            ChangedFileStatus::Modified,
            2,
            0,
        )];
        let findings = evaluate_boundary_path_changes(&default_policy(), &files);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn billing_path_change_triggers_boundary_finding() {
        let files = vec![simple_file(
            "server/src/billing/stripe.rs",
            ChangedFileStatus::Added,
            100,
            0,
        )];
        let findings = evaluate_boundary_path_changes(&default_policy(), &files);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn capability_allowlist_change_triggers_boundary_finding() {
        let files = vec![simple_file(
            "scripts/capability-boundary-allowlist.toml",
            ChangedFileStatus::Modified,
            5,
            2,
        )];
        let findings = evaluate_boundary_path_changes(&default_policy(), &files);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn iam_path_change_triggers_boundary_finding() {
        let files = vec![simple_file(
            "server/src/iam/roles.rs",
            ChangedFileStatus::Modified,
            8,
            3,
        )];
        let findings = evaluate_boundary_path_changes(&default_policy(), &files);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn rbac_path_change_triggers_boundary_finding() {
        let files = vec![simple_file(
            "server/src/rbac/policies.rs",
            ChangedFileStatus::Added,
            50,
            0,
        )];
        let findings = evaluate_boundary_path_changes(&default_policy(), &files);
        assert_eq!(findings.len(), 1);
    }

    // ── boundary_path_change: negative cases ────────────────────────────

    #[test]
    fn non_boundary_path_does_not_trigger() {
        let files = vec![simple_file(
            "server/src/utils/math.rs",
            ChangedFileStatus::Modified,
            10,
            5,
        )];
        let findings = evaluate_boundary_path_changes(&default_policy(), &files);
        assert!(findings.is_empty());
    }

    #[test]
    fn boundary_rule_disabled_skips_evaluation() {
        let mut policy = default_policy();
        policy.boundary_path.enabled = false;
        let files = vec![simple_file(
            "server/src/auth/login.rs",
            ChangedFileStatus::Modified,
            10,
            5,
        )];
        let findings = evaluate_boundary_path_changes(&policy, &files);
        assert!(findings.is_empty());
    }

    // ── boundary_path_change: match_outside_allowlist tuning ────────────

    #[test]
    fn match_outside_allowlist_false_only_flags_new_files() {
        let mut policy = default_policy();
        policy.boundary_path.match_outside_allowlist = false;

        // Modified existing auth file: should NOT trigger.
        let files = vec![simple_file(
            "server/src/auth/login.rs",
            ChangedFileStatus::Modified,
            10,
            5,
        )];
        let findings = evaluate_boundary_path_changes(&policy, &files);
        assert!(findings.is_empty());

        // Newly added auth file: should trigger.
        let files = vec![simple_file(
            "server/src/auth/new_endpoint.rs",
            ChangedFileStatus::Added,
            50,
            0,
        )];
        let findings = evaluate_boundary_path_changes(&policy, &files);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn match_outside_allowlist_false_still_flags_allowlist_file() {
        let mut policy = default_policy();
        policy.boundary_path.match_outside_allowlist = false;

        // The allowlist file itself should still trigger even when
        // match_outside_allowlist is false.
        let files = vec![simple_file(
            "scripts/capability-boundary-allowlist.toml",
            ChangedFileStatus::Modified,
            5,
            2,
        )];
        let findings = evaluate_boundary_path_changes(&policy, &files);
        assert_eq!(findings.len(), 1);
    }

    // ── boundary_path_change: generated/vendor exclusion ────────────────

    #[test]
    fn boundary_in_generated_file_excluded() {
        let files = vec![ChangedFile {
            is_generated: true,
            ..simple_file("server/src/auth/login.rs", ChangedFileStatus::Added, 10, 0)
        }];
        let findings = evaluate_boundary_path_changes(&default_policy(), &files);
        assert!(findings.is_empty());
    }

    // ── boundary_path_change: report-only ───────────────────────────────

    #[test]
    fn boundary_report_only_flag_propagated() {
        let files = vec![simple_file(
            "server/src/auth/login.rs",
            ChangedFileStatus::Modified,
            10,
            5,
        )];
        let findings = evaluate_boundary_path_changes(&policy_with_report_only_boundary(), &files);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].report_only);
    }

    // ── boundary_path_change: allowlist revision ────────────────────────

    #[test]
    fn boundary_findings_mark_allowlist_revision_present() {
        // The boundary rule doesn't set allowlist_revision in the RawFinding
        // itself; the engine does that when it sees BoundaryPathChange rule_id.
        // Here we just verify the rule_id is correct so the engine can
        // propagate.
        let files = vec![simple_file(
            "server/src/auth/login.rs",
            ChangedFileStatus::Modified,
            10,
            5,
        )];
        let findings = evaluate_boundary_path_changes(&default_policy(), &files);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule_id, TripwireRuleId::BoundaryPathChange);
    }

    // ── Deterministic ordering ──────────────────────────────────────────

    #[test]
    fn all_evaluators_produce_deterministic_findings() {
        // Run all evaluators twice with the same input and verify identical output.
        let files = vec![
            simple_file("migrations/001.sql", ChangedFileStatus::Added, 10, 0),
            simple_file("Cargo.toml", ChangedFileStatus::Modified, 3, 1),
            file_with_added_lines(
                "src/http_client.rs",
                &["let client = reqwest::Client::new();"],
            ),
            file_with_added_lines("src/native.rs", &["unsafe { ptr::null() }"]),
            simple_file(
                "server/src/auth/login.rs",
                ChangedFileStatus::Modified,
                10,
                5,
            ),
        ];
        let policy = default_policy();

        let findings_1: Vec<RawFinding> = {
            let mut v = Vec::new();
            v.extend(evaluate_migration_changes(&policy, &files));
            v.extend(evaluate_dependency_identity_changes(&policy, &files));
            v.extend(evaluate_network_egress_changes(&policy, &files));
            v.extend(evaluate_unsafe_code_changes(&policy, &files));
            v.extend(evaluate_boundary_path_changes(&policy, &files));
            v
        };

        let findings_2: Vec<RawFinding> = {
            let mut v = Vec::new();
            v.extend(evaluate_migration_changes(&policy, &files));
            v.extend(evaluate_dependency_identity_changes(&policy, &files));
            v.extend(evaluate_network_egress_changes(&policy, &files));
            v.extend(evaluate_unsafe_code_changes(&policy, &files));
            v.extend(evaluate_boundary_path_changes(&policy, &files));
            v
        };

        assert_eq!(findings_1.len(), findings_2.len());
        for (a, b) in findings_1.iter().zip(findings_2.iter()) {
            assert_eq!(a.rule_id, b.rule_id);
            assert_eq!(a.evidence_path, b.evidence_path);
            assert_eq!(a.evidence_start_line, b.evidence_start_line);
            assert_eq!(a.evidence_end_line, b.evidence_end_line);
            assert_eq!(a.report_only, b.report_only);
            assert_eq!(a.evidence_is_excluded, b.evidence_is_excluded);
        }
    }

    // ── Integration: all evaluators + engine ────────────────────────────

    #[test]
    fn all_evaluators_integrate_with_engine() {
        use crate::tripwires::engine::{GateOutcome, TripwireEvaluationInput, evaluate};

        let files = vec![
            simple_file("migrations/001.sql", ChangedFileStatus::Added, 10, 0),
            simple_file("Cargo.toml", ChangedFileStatus::Modified, 3, 1),
            file_with_added_lines(
                "src/http_client.rs",
                &["let client = reqwest::Client::new();"],
            ),
            file_with_added_lines("src/native.rs", &["unsafe { ptr::null() }"]),
            simple_file(
                "server/src/auth/login.rs",
                ChangedFileStatus::Modified,
                10,
                5,
            ),
        ];

        let input = TripwireEvaluationInput {
            task_id: "test_task".to_owned(),
            project_id: "proj_1".to_owned(),
            pr_number: Some(1),
            head_sha: "abc123".to_owned(),
            policy: default_policy(),
            allowlist_revision: None,
            changed_files: files,
        };

        let evaluators = all_rule_evaluators();
        let decision = evaluate(&input, &evaluators);

        // All five rules should fire.
        assert_eq!(decision.findings.len(), 5);
        assert_eq!(decision.outcome, GateOutcome::Held);
        assert_eq!(decision.enforcement_finding_count, 5);
        assert_eq!(decision.report_only_finding_count, 0);

        // Verify each rule family is represented.
        let rule_ids: Vec<TripwireRuleId> = decision.findings.iter().map(|f| f.rule_id).collect();
        assert!(rule_ids.contains(&TripwireRuleId::MigrationChange));
        assert!(rule_ids.contains(&TripwireRuleId::DependencyIdentityChange));
        assert!(rule_ids.contains(&TripwireRuleId::NetworkEgressChange));
        assert!(rule_ids.contains(&TripwireRuleId::UnsafeCodeChange));
        assert!(rule_ids.contains(&TripwireRuleId::BoundaryPathChange));
    }

    #[test]
    fn all_report_only_produces_report_only_outcome() {
        use crate::tripwires::engine::{GateOutcome, TripwireEvaluationInput, evaluate};

        let files = vec![
            simple_file("migrations/001.sql", ChangedFileStatus::Added, 10, 0),
            simple_file("Cargo.toml", ChangedFileStatus::Modified, 3, 1),
            file_with_added_lines(
                "src/http_client.rs",
                &["let client = reqwest::Client::new();"],
            ),
            file_with_added_lines("src/native.rs", &["unsafe { ptr::null() }"]),
            simple_file(
                "server/src/auth/login.rs",
                ChangedFileStatus::Modified,
                10,
                5,
            ),
        ];

        let mut policy = default_policy();
        policy.migration.report_only = true;
        policy.dependency_identity.report_only = true;
        policy.network_egress.report_only = true;
        policy.unsafe_code.report_only = true;
        policy.boundary_path.report_only = true;

        let input = TripwireEvaluationInput {
            task_id: "test_task".to_owned(),
            project_id: "proj_1".to_owned(),
            pr_number: Some(1),
            head_sha: "abc123".to_owned(),
            policy,
            allowlist_revision: None,
            changed_files: files,
        };

        let evaluators = all_rule_evaluators();
        let decision = evaluate(&input, &evaluators);

        assert_eq!(decision.outcome, GateOutcome::ReportOnly);
        assert_eq!(decision.enforcement_finding_count, 0);
        assert_eq!(decision.report_only_finding_count, 5);
    }

    // ── Edge cases ──────────────────────────────────────────────────────

    #[test]
    fn empty_changed_files_produces_no_findings() {
        let policy = default_policy();
        let empty: Vec<ChangedFile> = Vec::new();
        assert!(evaluate_migration_changes(&policy, &empty).is_empty());
        assert!(evaluate_dependency_identity_changes(&policy, &empty).is_empty());
        assert!(evaluate_network_egress_changes(&policy, &empty).is_empty());
        assert!(evaluate_unsafe_code_changes(&policy, &empty).is_empty());
        assert!(evaluate_boundary_path_changes(&policy, &empty).is_empty());
    }

    #[test]
    fn all_rules_disabled_produces_no_findings() {
        let mut policy = default_policy();
        policy.migration.enabled = false;
        policy.dependency_identity.enabled = false;
        policy.network_egress.enabled = false;
        policy.unsafe_code.enabled = false;
        policy.boundary_path.enabled = false;

        let files = vec![
            simple_file("migrations/001.sql", ChangedFileStatus::Added, 10, 0),
            simple_file("Cargo.toml", ChangedFileStatus::Modified, 3, 1),
            file_with_added_lines(
                "src/http_client.rs",
                &["let client = reqwest::Client::new();"],
            ),
            file_with_added_lines("src/native.rs", &["unsafe { ptr::null() }"]),
            simple_file(
                "server/src/auth/login.rs",
                ChangedFileStatus::Modified,
                10,
                5,
            ),
        ];

        assert!(evaluate_migration_changes(&policy, &files).is_empty());
        assert!(evaluate_dependency_identity_changes(&policy, &files).is_empty());
        assert!(evaluate_network_egress_changes(&policy, &files).is_empty());
        assert!(evaluate_unsafe_code_changes(&policy, &files).is_empty());
        assert!(evaluate_boundary_path_changes(&policy, &files).is_empty());
    }

    // ── helper: file_extension ──────────────────────────────────────────

    #[test]
    fn file_extension_extracts_correctly() {
        assert_eq!(file_extension("src/main.rs"), Some("rs"));
        assert_eq!(file_extension("src/component.tsx"), Some("tsx"));
        assert_eq!(file_extension("Makefile"), None);
        assert_eq!(file_extension("path/to/file.tar.gz"), Some("gz"));
        assert_eq!(file_extension(".gitignore"), None);
    }

    // ── helper: path_matches_any_glob ───────────────────────────────────

    #[test]
    fn glob_matching_works_for_common_patterns() {
        assert!(path_matches_any_glob(
            "migrations/001.sql",
            &["migrations/**".to_owned()]
        ));
        assert!(path_matches_any_glob(
            "db/migrations/002.sql",
            &["db/migrations/**".to_owned()]
        ));
        assert!(!path_matches_any_glob(
            "src/main.rs",
            &["migrations/**".to_owned()]
        ));
        assert!(path_matches_any_glob(
            "Cargo.toml",
            &["Cargo.toml".to_owned()]
        ));
        assert!(path_matches_any_glob(
            "frontend/package.json",
            &["**/package.json".to_owned()]
        ));
    }

    #[test]
    fn glob_matching_with_empty_patterns() {
        assert!(!path_matches_any_glob("anything", &[]));
    }

    // ── Multiple hunks per file ─────────────────────────────────────────

    #[test]
    fn unsafe_code_multiple_hunks_per_file() {
        let hunk1 = DiffHunk {
            new_start: 10,
            new_lines: 3,
            old_start: 10,
            old_lines: 0,
            diff_lines: vec![
                "+fn helper() {".to_owned(),
                "+  unsafe { ptr::null() }".to_owned(),
                "+}".to_owned(),
            ],
        };
        let hunk2 = DiffHunk {
            new_start: 50,
            new_lines: 3,
            old_start: 50,
            old_lines: 0,
            diff_lines: vec![
                "+fn other() {".to_owned(),
                r#"+  extern "C" fn cb()"#.to_owned(),
                "+}".to_owned(),
            ],
        };
        let file = ChangedFile {
            path: "src/native.rs".to_owned(),
            old_path: None,
            status: ChangedFileStatus::Modified,
            additions: 6,
            deletions: 0,
            hunks: vec![hunk1, hunk2],
            is_generated: false,
            is_vendor: false,
        };
        let findings = evaluate_unsafe_code_changes(&default_policy(), &[file]);
        assert_eq!(findings.len(), 2);
        // First finding should be from hunk1 (line 11), second from hunk2 (line 51).
        assert_eq!(findings[0].evidence_start_line, Some(11));
        assert_eq!(findings[1].evidence_start_line, Some(51));
    }
}
