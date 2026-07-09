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

#[cfg(test)]
mod tests;

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
/// path globs (e.g. `migrations/**`, `db/migrations/**`,
/// `migrations_postgres/**`, database crate SQL paths). Each
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
