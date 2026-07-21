//! Coordinator terminal-gate ownership boundary.
//!
//! `terminally_fail_task` is the only coordinator-dispatch exhaustion gateway
//! allowed to issue `ForceClose`. This is intentionally narrower than a global
//! `ForceClose` ban: PR-poller closure (`pr_poller/`), refinement completion
//! (`refinement_outcome.rs`), and supervisor/admin transitions have their own
//! lifecycle ownership and are outside this dispatch-exhaustion boundary.
//! `wave_dispatch.rs` is also excluded below, but only for its explicitly
//! documented post-push oversized-blob policy; it is not retry exhaustion or
//! autonomous remediation.

use std::path::Path;

const DISPATCH_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/dispatch");
const SESSION_RECOVERY_SRC: &str = include_str!("../dispatch/session_recovery.rs");
const TASK_DISPATCH_SRC: &str = include_str!("../dispatch/task_dispatch.rs");
const RETRY_SRC: &str = include_str!("../dispatch/retry.rs");
const WAVE_DISPATCH_SRC: &str = include_str!("../dispatch/wave_dispatch.rs");

/// The sole dispatch-local exception is a post-push policy: GitHub has already
/// rejected an oversized blob in an approved branch, so the task is escalated
/// to Planner and closed. Keeping this exception named and assertion-backed is
/// deliberate: it must not become an unreviewed allowlist for exhaustion code.
const WAVE_DISPATCH_LIFECYCLE_POLICY: &str =
    "push rejected: oversized blob in branch history; escalated to Planner to rewrite history";

#[test]
fn coordinator_exhaustion_force_close_has_one_terminal_gate() {
    let terminal_gate = function_source(SESSION_RECOVERY_SRC, "terminally_fail_task")
        .expect("session_recovery.rs must expose terminally_fail_task");

    assert_eq!(
        count_occurrences(SESSION_RECOVERY_SRC, "TransitionAction::ForceClose"),
        1,
        "session_recovery.rs may contain exactly one ForceClose, owned by terminally_fail_task"
    );
    assert!(
        terminal_gate.contains(".transition("),
        "terminally_fail_task must retain the repository transition gateway"
    );
    assert!(
        terminal_gate.contains("TransitionAction::ForceClose"),
        "terminally_fail_task must be the sole dispatch-exhaustion ForceClose gateway"
    );

    // These are the two current exhaustion owners: model-chain/max-retry
    // dispatch and the autonomous-remediation ceiling. They may call the gate,
    // but never the ForceClose transition directly.
    for (path, source) in [
        ("dispatch/task_dispatch.rs", TASK_DISPATCH_SRC),
        ("dispatch/retry.rs", RETRY_SRC),
    ] {
        assert!(
            source.contains("terminally_fail_task"),
            "{path} owns an exhaustion path and must route it through terminally_fail_task"
        );
        assert!(
            !source.contains("TransitionAction::ForceClose"),
            "{path} must not bypass terminally_fail_task with TransitionAction::ForceClose"
        );
    }

    // Catch new recovery/exhaustion modules as well. Test fixtures are excluded
    // because they only model transitions; production modules are all scanned.
    let mut offenders = dispatch_force_close_offenders(Path::new(DISPATCH_DIR));
    offenders.sort();
    assert!(
        offenders.is_empty(),
        "only dispatch/session_recovery.rs::terminally_fail_task may ForceClose exhaustion work; \
         offending dispatch recovery modules: {offenders:?}"
    );
}

#[test]
fn wave_dispatch_exception_is_explicitly_lifecycle_specific() {
    assert!(
        WAVE_DISPATCH_SRC.contains("TransitionAction::ForceClose")
            && WAVE_DISPATCH_SRC.contains(WAVE_DISPATCH_LIFECYCLE_POLICY),
        "the sole excluded dispatch ForceClose must remain the named oversized-blob post-push policy"
    );
}

#[test]
fn terminal_gate_uses_bounded_dispositions_without_high_cardinality_metric_labels() {
    let terminal_gate = function_source(SESSION_RECOVERY_SRC, "terminally_fail_task")
        .expect("session_recovery.rs must expose terminally_fail_task");
    let dispositions = quoted_field_values(terminal_gate, "pr_disposition");

    assert!(
        dispositions.contains(&"force_closed_no_pr"),
        "terminal gate must log the no-PR disposition"
    );
    assert!(
        dispositions.contains(&"pr_handoff"),
        "terminal gate must log the PR-handoff disposition"
    );
    assert!(
        dispositions
            .iter()
            .all(|value| matches!(*value, "force_closed_no_pr" | "pr_handoff")),
        "terminal-gate pr_disposition must be bounded to force_closed_no_pr|pr_handoff; found {dispositions:?}"
    );

    // Task IDs and snapshot SHAs remain tracing fields (useful diagnostics),
    // never metric dimensions. The gate has no direct metric-label API; adding
    // one requires a deliberate bounded telemetry design outside this gate.
    for metric_label_api in [
        "metrics::",
        "counter!(",
        "gauge!(",
        "histogram!(",
        "with_label_values",
    ] {
        assert!(
            !terminal_gate.contains(metric_label_api),
            "terminal gate must not create metric labels (especially task IDs or SHAs); found {metric_label_api}"
        );
    }
    for high_cardinality_dimension in [
        "task_id =>",
        "short_id =>",
        "sha =>",
        "head_sha =>",
        "snapshot_head_sha =>",
    ] {
        assert!(
            !terminal_gate.contains(high_cardinality_dimension),
            "terminal gate must not use {high_cardinality_dimension} as a metric-label dimension"
        );
    }
}

fn dispatch_force_close_offenders(dispatch_dir: &Path) -> Vec<String> {
    let entries =
        std::fs::read_dir(dispatch_dir).expect("read coordinator dispatch source directory");
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_production_dispatch_module(path))
        .filter_map(|path| {
            let source = std::fs::read_to_string(&path).expect("read coordinator dispatch source");
            source
                .contains("TransitionAction::ForceClose")
                .then(|| path.file_name().unwrap().to_string_lossy().into_owned())
        })
        .collect()
}

fn is_production_dispatch_module(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    path.extension().and_then(|extension| extension.to_str()) == Some("rs")
        && !name.ends_with("_tests.rs")
        && name != "session_recovery.rs"
        // See WAVE_DISPATCH_LIFECYCLE_POLICY: this is the one named policy
        // exception, not a generic bypass allowance.
        && name != "wave_dispatch.rs"
}

fn function_source<'a>(source: &'a str, function_name: &str) -> Option<&'a str> {
    let start = source.find(&format!("fn {function_name}"))?;
    let body_start = start + source[start..].find('{')?;
    let mut depth = 0usize;
    for (offset, byte) in source[body_start..].bytes().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&source[start..=body_start + offset]);
                }
            }
            _ => {}
        }
    }
    None
}

fn quoted_field_values<'a>(source: &'a str, field: &str) -> Vec<&'a str> {
    let marker = [field, r#" = ""#].concat();
    source
        .split(&marker)
        .skip(1)
        .filter_map(|tail| tail.split('"').next())
        .collect()
}

fn count_occurrences(source: &str, needle: &str) -> usize {
    source.match_indices(needle).count()
}
