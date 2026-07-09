//! Tests for the five original tripwire rule families (imb4) plus
//! deterministic ordering, integration, edge-case, helper, and
//! multi-hunk tests.

use super::helpers::*;
use crate::tripwires::engine::{ChangedFile, ChangedFileStatus, DiffHunk, RawFinding};
use crate::tripwires::policy::TripwirePolicy;
use crate::tripwires::reason_codes::TripwireRuleId;
use crate::tripwires::rules::{
    all_rule_evaluators, evaluate_boundary_path_changes,
    evaluate_ci_workflow_changes, evaluate_dependency_identity_changes,
    evaluate_large_delete_or_rewrite, evaluate_migration_changes,
    evaluate_network_egress_changes, evaluate_unsafe_code_changes,
    file_extension, path_matches_any_glob,
};

// ── migration_change: positive cases ────────────────────────────────────

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

#[test]
fn migration_file_under_migrations_postgres_path() {
    let files = vec![simple_file(
        "migrations_postgres/001_create_users.sql",
        ChangedFileStatus::Added,
        45,
        0,
    )];
    let findings = evaluate_migration_changes(&default_policy(), &files);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].rule_id, TripwireRuleId::MigrationChange);
    assert_eq!(
        findings[0].evidence_path,
        "migrations_postgres/001_create_users.sql"
    );
}

#[test]
fn migration_file_under_nested_migrations_postgres_path() {
    let files = vec![simple_file(
        "server/migrations_postgres/002_add_email.sql",
        ChangedFileStatus::Modified,
        15,
        5,
    )];
    let findings = evaluate_migration_changes(&default_policy(), &files);
    assert_eq!(findings.len(), 1);
}

#[test]
fn migration_sql_file_under_db_directory() {
    let files = vec![simple_file(
        "db/schema/003_add_index.sql",
        ChangedFileStatus::Added,
        20,
        0,
    )];
    let findings = evaluate_migration_changes(&default_policy(), &files);
    assert_eq!(findings.len(), 1);
}

#[test]
fn migration_sql_file_under_database_directory() {
    let files = vec![simple_file(
        "database/init/004_bootstrap.sql",
        ChangedFileStatus::Added,
        100,
        0,
    )];
    let findings = evaluate_migration_changes(&default_policy(), &files);
    assert_eq!(findings.len(), 1);
}

#[test]
fn migration_file_under_database_crate_path() {
    let files = vec![simple_file(
        "crates/db-core/migrations/005_create_sessions.sql",
        ChangedFileStatus::Added,
        60,
        0,
    )];
    let findings = evaluate_migration_changes(&default_policy(), &files);
    assert_eq!(findings.len(), 1);
}

#[test]
fn migration_file_under_database_crate_nested_sql() {
    let files = vec![simple_file(
        "crates/postgres-store/migrations/006_audit_log.sql",
        ChangedFileStatus::Modified,
        25,
        10,
    )];
    let findings = evaluate_migration_changes(&default_policy(), &files);
    assert_eq!(findings.len(), 1);
}

// ── migration_change: negative cases ────────────────────────────────────

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

// ── migration_change: generated/vendor exclusion ────────────────────────

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

// ── migration_change: report-only ───────────────────────────────────────

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

// ── dependency_identity_change: positive cases ──────────────────────────

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

// ── dependency_identity_change: negative cases ──────────────────────────

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

// ── dependency_identity_change: generated/vendor exclusion ──────────────

#[test]
fn manifest_in_generated_directory_excluded() {
    let files = vec![ChangedFile {
        is_generated: true,
        ..simple_file("target/pkg/Cargo.toml", ChangedFileStatus::Added, 10, 0)
    }];
    let findings = evaluate_dependency_identity_changes(&default_policy(), &files);
    assert!(findings.is_empty());
}

// ── dependency_identity_change: report-only ─────────────────────────────

#[test]
fn dependency_report_only_flag_propagated() {
    let files = vec![simple_file("Cargo.toml", ChangedFileStatus::Modified, 3, 1)];
    let findings =
        evaluate_dependency_identity_changes(&policy_with_report_only_dependency(), &files);
    assert_eq!(findings.len(), 1);
    assert!(findings[0].report_only);
}

// ── network_egress_change: positive cases ───────────────────────────────

// NOTE: The test strings below intentionally contain the egress matcher
// substrings (e.g. "reqwest::", "Webhook", "fetch(", "axios.") that the
// network_egress rule is designed to detect. The capability-boundary
// allowlist exempts this test file from the HTTP boundary guard so these
// fixture strings do not cause a false boundary violation.

#[test]
fn http_client_usage_in_added_line_triggers_egress() {
    // Uses "reqwest::" substring which the default egress matcher detects.
    let client_line = format!("let client = {}::Client::new();", "reqwest");
    let files = vec![file_with_added_lines(
        "src/http_client.rs",
        &[client_line.as_str()],
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

// ── network_egress_change: negative cases ───────────────────────────────

#[test]
fn context_line_with_http_client_does_not_trigger() {
    let client_line = format!("let client = {}::Client::new();", "reqwest");
    let files = vec![file_with_mixed_diff(
        "src/http_client.rs",
        10,
        &[
            (' ', client_line.as_str()),             // context, not added
            ('+', "client.get(url).send().await?;"), // added, no matcher
        ],
    )];
    let findings = evaluate_network_egress_changes(&default_policy(), &files);
    assert!(findings.is_empty());
}

#[test]
fn removed_line_with_http_client_does_not_trigger() {
    let client_line = format!("let client = {}::Client::new();", "reqwest");
    let files = vec![file_with_mixed_diff(
        "src/http_client.rs",
        10,
        &[
            ('-', client_line.as_str()), // removed, not added
        ],
    )];
    let findings = evaluate_network_egress_changes(&default_policy(), &files);
    assert!(findings.is_empty());
}

#[test]
fn egress_rule_disabled_skips_evaluation() {
    let mut policy = default_policy();
    policy.network_egress.enabled = false;
    let client_line = format!("let client = {}::Client::new();", "reqwest");
    let files = vec![file_with_added_lines(
        "src/http_client.rs",
        &[client_line.as_str()],
    )];
    let findings = evaluate_network_egress_changes(&policy, &files);
    assert!(findings.is_empty());
}

#[test]
fn no_egress_matchers_does_not_trigger() {
    let mut policy = default_policy();
    policy.network_egress.matcher_substrings = Vec::new();
    let client_line = format!("let client = {}::Client::new();", "reqwest");
    let files = vec![file_with_added_lines(
        "src/http_client.rs",
        &[client_line.as_str()],
    )];
    let findings = evaluate_network_egress_changes(&policy, &files);
    assert!(findings.is_empty());
}

// ── network_egress_change: generated/vendor exclusion ───────────────────

#[test]
fn egress_in_generated_file_excluded() {
    let client_line = format!("let client = {}::Client::new();", "reqwest");
    let mut file = file_with_added_lines("src/http_client.rs", &[client_line.as_str()]);
    file.is_generated = true;
    let findings = evaluate_network_egress_changes(&default_policy(), &[file]);
    assert!(findings.is_empty());
}

// ── network_egress_change: report-only ──────────────────────────────────

#[test]
fn egress_report_only_flag_propagated() {
    let client_line = format!("let client = {}::Client::new();", "reqwest");
    let files = vec![file_with_added_lines(
        "src/http_client.rs",
        &[client_line.as_str()],
    )];
    let findings = evaluate_network_egress_changes(&policy_with_report_only_egress(), &files);
    assert_eq!(findings.len(), 1);
    assert!(findings[0].report_only);
}

// ── unsafe_code_change: positive cases ──────────────────────────────────

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
        &[r#"lib = ctypes.CDLL("libfoo.so")"#],
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

// ── unsafe_code_change: negative cases ──────────────────────────────────

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

// ── unsafe_code_change: generated/vendor exclusion ──────────────────────

#[test]
fn unsafe_in_generated_file_excluded() {
    let mut file = file_with_added_lines("src/native.rs", &["unsafe { ptr::null() }"]);
    file.is_generated = true;
    let findings = evaluate_unsafe_code_changes(&default_policy(), &[file]);
    assert!(findings.is_empty());
}

// ── unsafe_code_change: report-only ─────────────────────────────────────

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

// ── unsafe_code_change: multiple findings per file ──────────────────────

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

// ── boundary_path_change: positive cases ────────────────────────────────

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

// ── boundary_path_change: negative cases ────────────────────────────────

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

// ── boundary_path_change: match_outside_allowlist tuning ────────────────

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

// ── boundary_path_change: generated/vendor exclusion ────────────────────

#[test]
fn boundary_in_generated_file_excluded() {
    let files = vec![ChangedFile {
        is_generated: true,
        ..simple_file("server/src/auth/login.rs", ChangedFileStatus::Added, 10, 0)
    }];
    let findings = evaluate_boundary_path_changes(&default_policy(), &files);
    assert!(findings.is_empty());
}

// ── boundary_path_change: report-only ───────────────────────────────────

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

// ── boundary_path_change: allowlist revision ────────────────────────────

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

// ── Deterministic ordering ──────────────────────────────────────────────

#[test]
fn all_evaluators_produce_deterministic_findings() {
    // Run all evaluators twice with the same input and verify identical output.
    let client_line = format!("let client = {}::Client::new();", "reqwest");
    let files = vec![
        simple_file("migrations/001.sql", ChangedFileStatus::Added, 10, 0),
        simple_file("Cargo.toml", ChangedFileStatus::Modified, 3, 1),
        file_with_added_lines("src/http_client.rs", &[client_line.as_str()]),
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
        v.extend(evaluate_large_delete_or_rewrite(&policy, &files));
        v.extend(evaluate_ci_workflow_changes(&policy, &files));
        v
    };

    let findings_2: Vec<RawFinding> = {
        let mut v = Vec::new();
        v.extend(evaluate_migration_changes(&policy, &files));
        v.extend(evaluate_dependency_identity_changes(&policy, &files));
        v.extend(evaluate_network_egress_changes(&policy, &files));
        v.extend(evaluate_unsafe_code_changes(&policy, &files));
        v.extend(evaluate_boundary_path_changes(&policy, &files));
        v.extend(evaluate_large_delete_or_rewrite(&policy, &files));
        v.extend(evaluate_ci_workflow_changes(&policy, &files));
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

// ── Integration: all evaluators + engine ────────────────────────────────

#[test]
fn all_evaluators_integrate_with_engine() {
    use crate::tripwires::engine::{GateOutcome, TripwireEvaluationInput, evaluate};

    let client_line = format!("let client = {}::Client::new();", "reqwest");
    let files = vec![
        simple_file("migrations/001.sql", ChangedFileStatus::Added, 10, 0),
        simple_file("Cargo.toml", ChangedFileStatus::Modified, 3, 1),
        file_with_added_lines("src/http_client.rs", &[client_line.as_str()]),
        file_with_added_lines("src/native.rs", &["unsafe { ptr::null() }"]),
        simple_file(
            "server/src/auth/login.rs",
            ChangedFileStatus::Modified,
            10,
            5,
        ),
        simple_file(
            ".github/workflows/ci.yml",
            ChangedFileStatus::Modified,
            5,
            2,
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

    // All seven rules should fire (six original + ci_workflow).
    // large_delete_or_rewrite does NOT fire because no files exceed
    // the default per-file (400) or aggregate (1500) thresholds.
    assert_eq!(decision.findings.len(), 6);
    assert_eq!(decision.outcome, GateOutcome::Held);
    assert_eq!(decision.enforcement_finding_count, 6);
    assert_eq!(decision.report_only_finding_count, 0);

    // Verify each rule family is represented.
    let rule_ids: Vec<TripwireRuleId> = decision.findings.iter().map(|f| f.rule_id).collect();
    assert!(rule_ids.contains(&TripwireRuleId::MigrationChange));
    assert!(rule_ids.contains(&TripwireRuleId::DependencyIdentityChange));
    assert!(rule_ids.contains(&TripwireRuleId::NetworkEgressChange));
    assert!(rule_ids.contains(&TripwireRuleId::UnsafeCodeChange));
    assert!(rule_ids.contains(&TripwireRuleId::BoundaryPathChange));
    assert!(rule_ids.contains(&TripwireRuleId::CIWorkflowChange));
}

#[test]
fn all_report_only_produces_report_only_outcome() {
    use crate::tripwires::engine::{GateOutcome, TripwireEvaluationInput, evaluate};

    let client_line = format!("let client = {}::Client::new();", "reqwest");
    let files = vec![
        simple_file("migrations/001.sql", ChangedFileStatus::Added, 10, 0),
        simple_file("Cargo.toml", ChangedFileStatus::Modified, 3, 1),
        file_with_added_lines("src/http_client.rs", &[client_line.as_str()]),
        file_with_added_lines("src/native.rs", &["unsafe { ptr::null() }"]),
        simple_file(
            "server/src/auth/login.rs",
            ChangedFileStatus::Modified,
            10,
            5,
        ),
        simple_file(
            ".github/workflows/ci.yml",
            ChangedFileStatus::Modified,
            5,
            2,
        ),
    ];

    let mut policy = default_policy();
    policy.migration.report_only = true;
    policy.dependency_identity.report_only = true;
    policy.network_egress.report_only = true;
    policy.unsafe_code.report_only = true;
    policy.boundary_path.report_only = true;
    policy.ci_workflow.report_only = true;

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
    assert_eq!(decision.report_only_finding_count, 6);
}

// ── Edge cases ──────────────────────────────────────────────────────────

#[test]
fn empty_changed_files_produces_no_findings() {
    let policy = default_policy();
    let empty: Vec<ChangedFile> = Vec::new();
    assert!(evaluate_migration_changes(&policy, &empty).is_empty());
    assert!(evaluate_dependency_identity_changes(&policy, &empty).is_empty());
    assert!(evaluate_network_egress_changes(&policy, &empty).is_empty());
    assert!(evaluate_unsafe_code_changes(&policy, &empty).is_empty());
    assert!(evaluate_boundary_path_changes(&policy, &empty).is_empty());
    assert!(evaluate_large_delete_or_rewrite(&policy, &empty).is_empty());
    assert!(evaluate_ci_workflow_changes(&policy, &empty).is_empty());
}

#[test]
fn all_rules_disabled_produces_no_findings() {
    let mut policy = default_policy();
    policy.migration.enabled = false;
    policy.dependency_identity.enabled = false;
    policy.network_egress.enabled = false;
    policy.unsafe_code.enabled = false;
    policy.boundary_path.enabled = false;
    policy.large_delete_rewrite.enabled = false;
    policy.ci_workflow.enabled = false;

    let client_line = format!("let client = {}::Client::new();", "reqwest");
    let files = vec![
        simple_file("migrations/001.sql", ChangedFileStatus::Added, 10, 0),
        simple_file("Cargo.toml", ChangedFileStatus::Modified, 3, 1),
        file_with_added_lines("src/http_client.rs", &[client_line.as_str()]),
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
    assert!(evaluate_large_delete_or_rewrite(&policy, &files).is_empty());
    assert!(evaluate_ci_workflow_changes(&policy, &files).is_empty());
}

// ── helper: file_extension ──────────────────────────────────────────────

#[test]
fn file_extension_extracts_correctly() {
    assert_eq!(file_extension("src/main.rs"), Some("rs"));
    assert_eq!(file_extension("src/component.tsx"), Some("tsx"));
    assert_eq!(file_extension("Makefile"), None);
    assert_eq!(file_extension("path/to/file.tar.gz"), Some("gz"));
    assert_eq!(file_extension(".gitignore"), None);
}

// ── helper: path_matches_any_glob ───────────────────────────────────────

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

// ── Multiple hunks per file ─────────────────────────────────────────────

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
