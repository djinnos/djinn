use std::path::Path;

use crate::context::AgentContext;
use crate::file_time::DestructiveClass;

pub(crate) mod build_drift;

use build_drift::{
    BUILD_DRIFT_STEER_MESSAGE, BuildDriftClassification, CanonicalCommand, classify_build_drift,
};

/// Investigation prompt shown to the worker on the FIRST covered edit per
/// (path, live-session). Demands the four facts the arbiter needs before
/// allowing mutation: importers/callers, affected public API, data shapes,
/// and the verbatim task instruction.
pub(crate) const GATEGUARD_FORCE_INVESTIGATION_PROMPT: &str = "\
GateGuard: Before you modify this file, provide:
1. Importers/callers of the code you are about to change.
2. Affected public functions, types, and traits.
3. Any data schema or shape that is touched.
4. The verbatim task instruction.
After providing this information, re-read the file and retry the edit.";

/// GateGuard edit-check helper for worker sessions.
///
/// Returns `Ok(())` to allow the mutation, or `Err(String)` to deny it.
/// Non-worker roles and missing roles pass through unconditionally.
///
/// Call this AFTER `file_time.assert(...)` succeeds AND after the successful
/// match byte range is known, but BEFORE writing the new content.
pub(crate) async fn gate_guard_edit_check(
    state: &AgentContext,
    session_role: Option<&str>,
    session_id: &str,
    path: &Path,
    mutation_byte_range: std::ops::Range<usize>,
) -> Result<(), String> {
    // Only gate worker sessions; all other roles keep current behavior.
    if session_role != Some("worker") {
        return Ok(());
    }

    // Look up the latest read record for coverage/truncation checks.
    if let Some(record) = state.file_time.latest_record(session_id, path).await {
        // Convert usize byte range to u64 for covers_span.
        let span_start = mutation_byte_range.start as u64;
        let span_end = mutation_byte_range.end as u64;

        if record.truncated {
            // Truncated read: deny, do NOT mark edit_forced, and record the
            // truncated diagnostic so repeated denials are consistent.
            return Err(format!(
                "FORCE-TRUNCATED-READ: The latest read of {} was truncated \
                 and did not observe the full file. You MUST re-read the \
                 entire file before editing.",
                path.display(),
            ));
        }

        if !record.covers_span(span_start, span_end) {
            // Uncovered span: same denial as truncated — the worker hasn't
            // seen the area it's trying to mutate.
            return Err(format!(
                "FORCE-UNCOVERED-READ: The latest read of {} does not cover \
                 the byte range [{}, {}) you are trying to edit. You MUST \
                 re-read the file with full coverage before editing.",
                path.display(),
                span_start,
                span_end,
            ));
        }

        // Covered, non-truncated read: check first-edit investigation gate.
        if !state.file_time.has_edit_forced(session_id, path).await {
            state.file_time.mark_edit_forced(session_id, path).await;
            return Err(GATEGUARD_FORCE_INVESTIGATION_PROMPT.to_string());
        }

        // edit_forced already set — allow this and all subsequent edits.
        Ok(())
    } else {
        // No read record at all should have been caught by the earlier
        // `file_time.assert(...)` call, but guard defensively.
        Ok(())
    }
}

/// Flatten the resolved `lifecycle.final_verification.command_groups` into the
/// reduced [`CanonicalCommand`] form the build-drift gate compares against.
///
/// Every command in every group is included (path selection rules are not
/// applied): the gate steers a divergent ad-hoc build toward
/// `run_verification` whenever the observed executable matches *any* canonical
/// build command, regardless of which selection rule would ultimately run it.
fn canonical_commands_from_config(
    config: &djinn_stack::environment::EnvironmentConfig,
) -> Vec<CanonicalCommand> {
    config
        .lifecycle
        .final_verification
        .command_groups
        .iter()
        .flat_map(|group| group.commands.iter())
        .map(|cmd| CanonicalCommand::new(&cmd.executable, cmd.argv.clone()))
        .collect()
}

/// Advisory build-drift soft gate (ri23 Part 1).
///
/// Resolves the worktree's owning project + canonical verification plan, then
/// classifies `command` against it. The gate:
///
/// - is **inert** (no telemetry, always allow) when no canonical plan is
///   configured;
/// - fails **open** (allow, `ineligible_pass` telemetry with an enumerated
///   reason) for any non-simple command shape;
/// - allows exact canonical re-runs (`argv_equal_pass`) and unrelated commands
///   (`unrelated_pass`);
/// - on the FIRST divergent (drift) command per `(session, project)`, denies
///   once with an advisory steer message (`drift_deny`) and records the drift
///   key in the per-session `bash_soft_forced` set; every repeat of the same
///   key thereafter passes (`repeat_pass`).
///
/// Never hard-blocks: a retry of the same command always proceeds.
async fn build_drift_shell_gate(
    state: &AgentContext,
    session_id: &str,
    worktree_path: &Path,
    command: &str,
) -> Result<(), String> {
    let resolved =
        crate::environment::resolved_project_environment_for_path(&state.db, worktree_path).await;
    let canonical = canonical_commands_from_config(&resolved.config);
    apply_build_drift_decision(
        state,
        session_id,
        command,
        resolved.project_id.as_deref(),
        &canonical,
    )
    .await
}

/// The DB-free core of [`build_drift_shell_gate`]: classify `command` against
/// the already-resolved `canonical` plan and apply per-`(session, project)`
/// deny-once bookkeeping + bounded telemetry.
///
/// Split out so tests can drive the full decision table (including deny-once /
/// repeat-pass) with an in-memory [`crate::file_time::FileTime`] and canonical
/// commands supplied directly, without provisioning a project row.
pub(crate) async fn apply_build_drift_decision(
    state: &AgentContext,
    session_id: &str,
    command: &str,
    project_id: Option<&str>,
    canonical: &[CanonicalCommand],
) -> Result<(), String> {
    use djinn_telemetry::build_drift as tel;

    match classify_build_drift(command, project_id, canonical) {
        // No canonical plan configured — the gate does nothing at all.
        BuildDriftClassification::Inert => Ok(()),
        BuildDriftClassification::Ineligible(reason) => {
            tel::increment_ineligible(reason.as_str());
            tel::increment_outcome(tel::OUTCOME_INELIGIBLE_PASS);
            Ok(())
        }
        BuildDriftClassification::ArgvEqual => {
            tel::increment_outcome(tel::OUTCOME_ARGV_EQUAL_PASS);
            Ok(())
        }
        BuildDriftClassification::Unrelated => {
            tel::increment_outcome(tel::OUTCOME_UNRELATED_PASS);
            Ok(())
        }
        BuildDriftClassification::Drift { key } => {
            if state.file_time.has_bash_soft_forced(session_id, &key).await {
                // This exact drift key already steered once this session —
                // let the retry through.
                tel::increment_outcome(tel::OUTCOME_REPEAT_PASS);
                Ok(())
            } else {
                state.file_time.mark_bash_soft_forced(session_id, key).await;
                tel::increment_outcome(tel::OUTCOME_DRIFT_DENY);
                tracing::info!(
                    project_id = project_id.unwrap_or(""),
                    "build-drift soft gate steered an ad-hoc build toward run_verification"
                );
                Err(BUILD_DRIFT_STEER_MESSAGE.to_string())
            }
        }
    }
}

/// Map a classifier `ShellDestructiveClass` label to the colocated
/// `DestructiveClass` used by `FileTime`'s `bash_soft_forced` set.
///
/// The classifier's `as_str()` labels are documented to match
/// `destructive_class::*` constants 1:1; `DestructiveClass::from_owned`
/// interns the static label so equality/hashing works across code paths.
fn map_shell_class_to_destructive_class(
    class: djinn_mcp_extension::command_classifier::ShellDestructiveClass,
) -> DestructiveClass {
    DestructiveClass::from_owned(class.as_str().to_owned())
}

/// GateGuard shell-check helper for worker sessions.
///
/// Returns `Ok(())` to allow shell execution, or `Err(String)` to deny it.
/// Non-worker roles and missing roles pass through unconditionally.
///
/// Call this AFTER cargo steering (`cargo_check_denied`) and BEFORE shell
/// process construction/execution.
///
/// `worktree_path` identifies the live session (its display string is the
/// `bash_soft_forced` map key) and is also resolved to the owning project's
/// canonical verification plan for the advisory build-drift soft gate.
pub(crate) async fn gate_guard_shell_check(
    state: &AgentContext,
    session_role: Option<&str>,
    worktree_path: &Path,
    command: &str,
) -> Result<(), String> {
    // Only gate worker sessions; all other roles keep current behavior.
    if session_role != Some("worker") {
        return Ok(());
    }

    let session_id = worktree_path.display().to_string();

    use djinn_mcp_extension::command_classifier::{
        ShellDestructiveDecision, classify_destructive_shell_command,
    };

    match classify_destructive_shell_command(command) {
        // The destructive classifier lets ordinary builds through. The
        // advisory build-drift soft gate runs only on this branch: it never
        // hard-blocks, only steers the first ad-hoc compile-producing command
        // per (session, project) toward `run_verification`.
        ShellDestructiveDecision::Allow => {
            build_drift_shell_gate(state, &session_id, worktree_path, command).await
        }
        ShellDestructiveDecision::HardDeny { reason } => {
            // Hard-denied commands are unconditionally forbidden.
            // Never recorded in bash_soft_forced — no retry will help.
            Err(format!(
                "GateGuard: This shell command is forbidden and cannot be \
                 unlocked by FORCE or retry. {reason}"
            ))
        }
        ShellDestructiveDecision::SoftGate { class, reason } => {
            let dc = map_shell_class_to_destructive_class(class);
            if state.file_time.has_bash_soft_forced(&session_id, &dc).await {
                // Already forced for this class in this live session — allow.
                Ok(())
            } else {
                // First occurrence: mark it and return a FORCE prompt.
                state.file_time.mark_bash_soft_forced(&session_id, dc).await;
                Err(format!(
                    "GateGuard: This shell command is gated — {reason}. \
                     Before executing, provide:\n\
                     1. What files or data will be deleted or mutated.\n\
                     2. A one-line rollback or recreation plan.\n\
                     After providing this information, retry the command."
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension::handlers::workspace::{
        call_apply_patch, call_edit, call_read, call_write,
    };
    use crate::file_time::ReadCoverage;
    use crate::test_helpers::{agent_context_from_db, create_test_db};
    use tokio_util::sync::CancellationToken;

    fn setup_worktree(prefix: &str) -> (tempfile::TempDir, AgentContext) {
        let dir = crate::test_helpers::test_tempdir(prefix);
        let db = create_test_db();
        let state = agent_context_from_db(db, CancellationToken::new());
        (dir, state)
    }

    mod edit_tests;
    mod patch_tests;
    mod write_tests;
}
