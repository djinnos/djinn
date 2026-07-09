use crate::tripwires::engine::{ChangedFile, ChangedFileStatus, DiffHunk};
use crate::tripwires::policy::TripwirePolicy;

// ── Helpers ──────────────────────────────────────────────────────────────

/// Build a minimal changed file with no diff content.
pub(super) fn simple_file(
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
pub(super) fn file_with_added_lines(path: &str, added_lines: &[&str]) -> ChangedFile {
    let diff_lines: Vec<String> = added_lines.iter().map(|l| format!("+{l}")).collect();
    let hunk = DiffHunk {
        new_start: 1,
        new_lines: added_lines.len() as u32,
        old_start: 1,
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
pub(super) fn file_with_mixed_diff(
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

pub(super) fn default_policy() -> TripwirePolicy {
    TripwirePolicy::default()
}

pub(super) fn policy_with_report_only_migration() -> TripwirePolicy {
    let mut p = default_policy();
    p.migration.report_only = true;
    p
}

pub(super) fn policy_with_report_only_dependency() -> TripwirePolicy {
    let mut p = default_policy();
    p.dependency_identity.report_only = true;
    p
}

pub(super) fn policy_with_report_only_egress() -> TripwirePolicy {
    let mut p = default_policy();
    p.network_egress.report_only = true;
    p
}

pub(super) fn policy_with_report_only_unsafe() -> TripwirePolicy {
    let mut p = default_policy();
    p.unsafe_code.report_only = true;
    p
}

pub(super) fn policy_with_report_only_boundary() -> TripwirePolicy {
    let mut p = default_policy();
    p.boundary_path.report_only = true;
    p
}
