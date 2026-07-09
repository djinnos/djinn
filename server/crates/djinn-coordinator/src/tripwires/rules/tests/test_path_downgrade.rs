//! Tests for the test-path severity downgrade: a finding whose evidence path
//! is a test file per djinn-core's `is_test_path` is emitted as report-only
//! (advisory) instead of enforcement, across every rule family. This kills the
//! dumbest tripwire false-positive class — an `unsafe`/egress matcher or a
//! large delete inside a test fixture is never a real enforcement hold.

use super::helpers::*;
use crate::tripwires::engine::ChangedFileStatus;
use crate::tripwires::rules::{
    evaluate_boundary_path_changes, evaluate_dependency_identity_changes,
    evaluate_large_delete_or_rewrite, evaluate_migration_changes, evaluate_network_egress_changes,
    evaluate_unsafe_code_changes,
};

/// Unsafe code inside a `_test.rs` file downgrades to report-only.
#[test]
fn unsafe_in_test_file_is_report_only() {
    let files = vec![file_with_added_lines(
        "server/crates/foo/src/ffi_test.rs",
        &["unsafe { ptr::read(addr); }"],
    )];
    let findings = evaluate_unsafe_code_changes(&default_policy(), &files);
    assert_eq!(findings.len(), 1);
    assert!(
        findings[0].report_only,
        "unsafe finding in a test file must be report-only"
    );
}

/// Unsafe code in a NON-test file still enforces (control).
#[test]
fn unsafe_in_production_file_still_enforces() {
    let files = vec![file_with_added_lines(
        "server/crates/foo/src/ffi.rs",
        &["unsafe { ptr::read(addr); }"],
    )];
    let findings = evaluate_unsafe_code_changes(&default_policy(), &files);
    assert_eq!(findings.len(), 1);
    assert!(
        !findings[0].report_only,
        "unsafe finding in production code must still enforce"
    );
}

/// Network egress inside a conventional `tests/` dir downgrades. Uses the
/// `Webhook` egress matcher (not an http-client crate path) so this fixture
/// string does not trip the repo's http capability-boundary detector.
#[test]
fn egress_in_tests_dir_is_report_only() {
    let files = vec![file_with_added_lines(
        "tests/integration_client.rs",
        &["Webhook::register(endpoint);"],
    )];
    let findings = evaluate_network_egress_changes(&default_policy(), &files);
    assert_eq!(findings.len(), 1);
    assert!(
        findings[0].report_only,
        "egress finding in a tests/ dir must be report-only"
    );
}

/// A large delete inside a test file downgrades.
#[test]
fn large_delete_in_test_file_is_report_only() {
    let files = vec![simple_file(
        "server/crates/foo/src/big_test.rs",
        ChangedFileStatus::Modified,
        10,
        600, // exceeds default per-file threshold
    )];
    let findings = evaluate_large_delete_or_rewrite(&default_policy(), &files);
    assert!(!findings.is_empty());
    assert!(
        findings.iter().all(|f| f.report_only),
        "large delete in a test file must be report-only"
    );
}

/// A boundary-path change under a test dir downgrades.
#[test]
fn boundary_change_in_test_dir_is_report_only() {
    let files = vec![simple_file(
        "tests/auth/permissions_fixture.rs",
        ChangedFileStatus::Added,
        40,
        0,
    )];
    let findings = evaluate_boundary_path_changes(&default_policy(), &files);
    assert_eq!(findings.len(), 1);
    assert!(
        findings[0].report_only,
        "boundary change in a test dir must be report-only"
    );
}

/// A dependency manifest inside a test dir downgrades.
#[test]
fn dependency_manifest_in_test_dir_is_report_only() {
    let files = vec![simple_file(
        "tests/fixtures/Cargo.toml",
        ChangedFileStatus::Modified,
        3,
        1,
    )];
    let findings = evaluate_dependency_identity_changes(&default_policy(), &files);
    assert_eq!(findings.len(), 1);
    assert!(
        findings[0].report_only,
        "dependency manifest under tests/ must be report-only"
    );
}

/// A migration `.sql` under a test dir downgrades.
#[test]
fn migration_sql_in_test_dir_is_report_only() {
    let files = vec![simple_file(
        "tests/db/schema_fixture.sql",
        ChangedFileStatus::Added,
        20,
        0,
    )];
    let findings = evaluate_migration_changes(&default_policy(), &files);
    assert_eq!(findings.len(), 1);
    assert!(
        findings[0].report_only,
        "migration sql under a test dir must be report-only"
    );
}
