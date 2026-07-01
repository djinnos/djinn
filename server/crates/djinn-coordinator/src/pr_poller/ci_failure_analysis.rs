use super::*;

/// Compute a stable fingerprint from normalized failing-check names and CI
/// failure sections. The fingerprint is a hash of:
/// 1. Sorted, deduplicated failing check-run names (normalized: lowercase, trimmed)
/// 2. The workflow names + failed job names + failed step names from the CI
///    failure sections (the structured content, not the full text)
///
/// Returns a hex string. Two CI failures with the same fingerprint indicate
/// the worker is hitting the exact same checks/errors across pushes.
pub(crate) fn compute_ci_failure_fingerprint(
    failed_checks: &[&CheckRun],
    ci_failure_sections: &[String],
) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut check_names: Vec<String> = failed_checks
        .iter()
        .map(|cr| cr.name.to_lowercase().trim().to_string())
        .collect();
    check_names.sort();
    check_names.dedup();

    let mut failure_markers: Vec<String> = ci_failure_sections
        .iter()
        .filter(|s| s.starts_with("**Failed job:**") || s.starts_with("**Failed step:**"))
        .cloned()
        .collect();
    failure_markers.sort();
    failure_markers.dedup();

    let combined = format!(
        "checks:{}|failures:{}",
        check_names.join(","),
        failure_markers.join(",")
    );
    let mut hasher = DefaultHasher::new();
    combined.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

/// Walk activity entries in reverse chronological order and count how many
/// consecutive entries have a fingerprint matching `current_fp`.
/// Stops at the first different fingerprint.
#[cfg(test)]
pub(crate) fn count_consecutive_identical(
    entries: &[djinn_core::models::ActivityEntry],
    current_fp: &str,
) -> u32 {
    let mut count = 0u32;
    for entry in entries.iter().rev() {
        let parsed: serde_json::Value = match serde_json::from_str(&entry.payload) {
            Ok(v) => v,
            Err(_) => break,
        };
        let fp = match parsed.get("fingerprint").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => break,
        };
        if fp == current_fp {
            count += 1;
        } else {
            break;
        }
    }
    count
}

/// Determine whether a CI failure is a scope-inversion: the failing crates/files
/// are OUTSIDE the PR's own git diff.
pub(crate) fn detect_scope_inversion(
    ci_failure_sections: &[String],
    pr_files: &[String],
) -> Option<bool> {
    if pr_files.is_empty() {
        return None;
    }

    let failing_crates = extract_crate_names_from_sections(ci_failure_sections);
    if failing_crates.is_empty() {
        return None;
    }

    let pr_crates = extract_crate_names(pr_files);
    if pr_crates.is_empty() {
        return None;
    }

    let pr_crate_set: std::collections::HashSet<&str> =
        pr_crates.iter().map(|s| s.as_str()).collect();
    let any_outside = failing_crates
        .iter()
        .any(|c| !pr_crate_set.contains(c.as_str()));

    Some(any_outside)
}

/// Extract crate names from a list of file paths using a simple heuristic:
/// - `server/crates/<crate-name>/src/...` → `<crate-name>`
/// - `crates/<crate-name>/src/...` → `<crate-name>`
/// - Paths without `crates/` return `None`.
pub(crate) fn extract_crate_name(path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('/').collect();
    for i in 0..parts.len().saturating_sub(1) {
        if parts[i] == "crates" {
            return parts.get(i + 1).map(|s| s.to_string());
        }
    }
    None
}

/// Extract crate names from a list of file paths, deduplicated and sorted.
pub(crate) fn extract_crate_names(paths: &[String]) -> Vec<String> {
    let mut crates: Vec<String> = paths.iter().filter_map(|p| extract_crate_name(p)).collect();
    crates.sort();
    crates.dedup();
    crates
}

/// Extract crate names from CI failure sections by looking for file paths
/// embedded in the failure text.
pub(crate) fn extract_crate_names_from_sections(sections: &[String]) -> Vec<String> {
    let mut crates = std::collections::HashSet::new();
    for section in sections {
        for line in section.lines() {
            if let Some(arrow_idx) = line.find("-->") {
                let trimmed = line[arrow_idx + 3..].trim();
                let path_part = rust_error_path(trimmed);
                if let Some(crate_name) = extract_crate_name(path_part) {
                    crates.insert(crate_name);
                }
            }
        }

        if let Some(start) = section.find("crates/") {
            let after = &section[start + 7..];
            if let Some(end) = after.find('/') {
                let crate_name = &after[..end];
                if !crate_name.is_empty() {
                    crates.insert(crate_name.to_string());
                }
            }
        }
    }
    let mut result: Vec<String> = crates.into_iter().collect();
    result.sort();
    result
}

fn rust_error_path(trimmed: &str) -> &str {
    let Some(last_colon) = trimmed.rfind(':') else {
        return trimmed;
    };
    let before_last_colon = &trimmed[..last_colon];
    let Some(prev_colon) = before_last_colon.rfind(':') else {
        return trimmed;
    };
    let candidate = &trimmed[..prev_colon];
    if candidate.contains('/') {
        candidate
    } else {
        trimmed
    }
}
