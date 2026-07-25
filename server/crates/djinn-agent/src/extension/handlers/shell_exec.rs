//! Shell-invocation timeout policy and the single cargo-observation seam.
//!
//! Split out of `workspace.rs`, which was within a hundred bytes of the 51200
//! byte size guard. Everything here is about *how one shell command is timed
//! and observed*, which is a smaller and much more testable concern than the
//! workspace tool surface that calls it.

use djinn_core::clock::Clock;
use djinn_telemetry::cargo_invocation::{EXIT_CANCELLED, EXIT_FAIL, EXIT_OK};

/// Default interactive-shell timeout (ms) when the caller passes no `timeout_ms`.
/// Overridable via `DJINN_SHELL_TIMEOUT_MS`. Raised well above the old 120s:
/// cold native builds routinely exceed two minutes, and a too-short ceiling
/// SIGKILLed compiles mid-flight, leaving the model to retry from cold — the
/// same guillotine the command runner already fixed.
fn default_shell_timeout_ms() -> u64 {
    std::env::var("DJINN_SHELL_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v >= 1000)
        .unwrap_or(600_000)
}

/// Minimum timeout (ms) enforced for slow native build/test commands, even when
/// the caller requests a smaller value. A cold `cargo`/`clippy`/`nextest`/`go`/
/// `pnpm` compile legitimately runs many minutes; flooring stops a low guess
/// from killing a build that is still making progress. Overridable via
/// `DJINN_SHELL_BUILD_TIMEOUT_MS`. Only ever RAISES the effective timeout.
fn build_command_floor_ms() -> u64 {
    std::env::var("DJINN_SHELL_BUILD_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v >= 1000)
        .unwrap_or(1_800_000)
}

/// Heuristic: does this command invoke a slow native build/test toolchain whose
/// cold compile can exceed the default timeout? Matches on substrings so the
/// common `cd server && cargo …` shape is covered. False positives are benign:
/// a non-build command that happens to contain the needle finishes fast anyway,
/// so the only effect is a higher (unused) ceiling.
fn is_build_command(command: &str) -> bool {
    const NEEDLES: [&str; 8] = [
        "cargo ", "nextest", "go build", "go test", "pnpm ", "npm run", "make ", "bazel ",
    ];
    NEEDLES.iter().any(|needle| command.contains(needle))
}

/// Resolve the effective shell timeout: the caller's `timeout_ms` (or the
/// default), clamped to a sane minimum, then floored up for build/test commands.
///
/// Deliberately role-blind, exactly like the invocation lease: a Reviewer's
/// compile gets the same build floor as a Worker's, because the compile takes
/// the same wall-clock either way.
pub(super) fn effective_shell_timeout_ms(requested: Option<u64>, command: &str) -> u64 {
    let base = requested.unwrap_or_else(default_shell_timeout_ms).max(1000);
    if is_build_command(command) {
        base.max(build_command_floor_ms())
    } else {
        base
    }
}

/// Structurally record exactly one cargo invocation observation from a single
/// runner terminal result.
///
/// This is the private testable seam between the process runner and the
/// telemetry contract. Exactly-once is structural: there is exactly one call
/// site in `call_shell`, placed after the single runner return. No Drop
/// guard, no recordings in individual timeout/cancellation branches.
///
/// Mapping:
/// - `classification == None` (non-cargo command): no observation.
/// - [`crate::process::ProcessRunError::Spawn`] (child never started): no observation.
/// - Successful exit ([`crate::process::ProcessTermination::Exited`] + success): `EXIT_OK`.
/// - Nonzero exit, timeout, or post-start runner error: `EXIT_FAIL`.
/// - Handled cancellation: `EXIT_CANCELLED`.
///
/// `class` is the session role's [`djinn_runtime::RoleResourceClass`] rendered
/// via `as_str()` — a two-valued, bounded label. It exists to answer the one
/// question that blocks arming the invocation semaphore: *what fraction of
/// observed cargo invocations came from a role dispatch never charged a build
/// slot?* It labels the observation only; nothing downstream branches on it.
pub(super) fn finish_shell(
    classification: Option<&'static str>,
    class: &'static str,
    started: std::time::Instant,
    result: &Result<crate::process::ProcessOutput, crate::process::ProcessRunError>,
    clock: &dyn Clock,
    recorder: impl Fn(&'static str, &'static str, &'static str, std::time::Duration),
) {
    let Some(kind) = classification else {
        return;
    };
    let exit: &'static str = match result {
        // Spawn error: child never started — no observation.
        Err(crate::process::ProcessRunError::Spawn(_)) => return,
        // Post-start runner error (wait/reap/join): child started.
        Err(crate::process::ProcessRunError::Started(_)) => EXIT_FAIL,
        Ok(po) => match po.termination {
            // Handled cancellation: child started and was cleaned up.
            crate::process::ProcessTermination::Cancelled => EXIT_CANCELLED,
            // Timeout: child was killed by the deadline — always fail.
            crate::process::ProcessTermination::TimedOut => EXIT_FAIL,
            crate::process::ProcessTermination::Exited if po.output.status.success() => EXIT_OK,
            crate::process::ProcessTermination::Exited => EXIT_FAIL,
        },
    };
    let ended = clock.now_instant();
    let elapsed = ended.saturating_duration_since(started);
    recorder(kind, exit, class, elapsed);
}

#[cfg(all(test, unix))]
#[path = "workspace_cargo_outcome_tests.rs"]
mod cargo_outcome_tests;

#[cfg(test)]
mod timeout_tests {
    use super::{effective_shell_timeout_ms, is_build_command};

    #[test]
    fn build_commands_are_detected() {
        assert!(is_build_command("cd server && cargo check -p djinn-db"));
        assert!(is_build_command("cargo clippy --all-features"));
        assert!(is_build_command("cargo nextest run"));
        assert!(is_build_command("go test ./..."));
        assert!(is_build_command("pnpm install"));
        assert!(is_build_command("make build"));
        assert!(!is_build_command("ls -la"));
        assert!(!is_build_command("git status"));
        assert!(!is_build_command("grep -r foo src"));
    }

    #[test]
    fn build_commands_are_floored_above_a_small_request() {
        // A 120s request for a cold compile must be raised to the build floor,
        // not honored verbatim (that was the SIGKILL-mid-build bug).
        let got = effective_shell_timeout_ms(Some(120_000), "cargo build");
        assert!(got >= 1_800_000, "build floor not applied: {got}");
    }

    #[test]
    fn non_build_commands_keep_the_requested_timeout() {
        assert_eq!(effective_shell_timeout_ms(Some(5_000), "echo hi"), 5_000);
    }

    #[test]
    fn requests_are_clamped_to_a_sane_minimum() {
        assert_eq!(effective_shell_timeout_ms(Some(10), "echo hi"), 1000);
    }

    #[test]
    fn a_large_explicit_build_timeout_is_preserved() {
        let got = effective_shell_timeout_ms(Some(3_600_000), "cargo test");
        assert_eq!(got, 3_600_000);
    }
}
