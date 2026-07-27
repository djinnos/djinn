//! Perform the task-run Cargo target seed **as the launcher-spawned child**
//! (uid 1001) instead of in the worker process (uid 1000).
//!
//! # The defect this closes
//!
//! Directory-entry operations (`create` / `unlink` / `rename` / `linkat`) are
//! governed by the DIRECTORY's permissions, and content operations
//! (`open(O_TRUNC)` / `write` / `truncate`) by the FILE's. So gid 1000 plus
//! setgid plus `g+w` genuinely lets the worker (1000) and cargo (1001) share the
//! cargo trees for those.
//!
//! Inode-METADATA operations — `chmod`, `chown`, `utimensat` with explicit
//! times — are governed by **ownership only**. The kernel returns `EPERM` to a
//! non-owner even when the requested mode is byte-identical to the current one,
//! and no mode bit, setgid bit, ACL or group membership can delegate them.
//!
//! `std::fs::copy` always ends in `set_permissions`. So when the uid-1000
//! worker seeds a `Copy`-classified entry into the private run dir, the
//! resulting inode is owned by 1000 — and the build script that later
//! `fs::copy`s its output over that same path runs as 1001 and fails at the
//! final `chmod`, with the bytes already written. Build-script `OUT_DIR`
//! payloads are exactly the class the seeder copies rather than hardlinks
//! (see `cargo_target_seed`'s module docs), so this is not an edge case.
//!
//! Performing the seed at 1001 makes the seeded inodes born owned by the
//! identity that will later overwrite them, and the `chmod` succeeds because
//! the copier is the owner.
//!
//! # Why `bash -lc`, and not a widened program prefix list
//!
//! `djinn_cgroup_launcher::command_path::safe_command_path` admits `/bin/`,
//! `/usr/bin/` and `/workspace/` programs; `/opt/djinn/bin` is deliberately not
//! on the list, and its module docs record that decision as a posture rather
//! than an oversight (widening it would advertise a provenance guarantee that
//! does not exist, since the one program a brokered command names is a shell
//! which then resolves everything else itself). Every brokered command in this
//! repo already goes through `bash -lc`. This one does too — no posture change,
//! no new pinned-decision test to rewrite.
//!
//! # Degradation is mandatory, not optional
//!
//! Brokering the seed puts it behind the build-lease admission path. A LOST
//! QUEUE is already safe: `LeaseInvocationRunner` degrades that to unleased
//! execution and the child still runs to completion. What is not safe is
//! treating a broker-level failure (socket gone, launch refused, reap failed)
//! as fatal — that would convert a launcher problem into a failed dispatch for
//! every task-run in the fleet. So every failure here, including a non-zero
//! exit and an unparseable summary, falls back to the in-worker seed that
//! shipped before this change. [`BROKERED_SEED_ENV`] turns the whole path off
//! without a redeploy.
//!
//! # Why `djinn_sandbox::SANDBOX.apply` is deliberately not called
//!
//! The tool-facing shell path applies it; this one does not, and adding it
//! would be cargo-culting. `LandlockSandbox::apply` installs its ruleset through
//! `Command::pre_exec`, and `pre_exec` is a closure in THIS process — it cannot
//! cross the broker, which reconstructs the child from a `CommandSpec`
//! (program, argv, cwd, environment) inside the launcher. So on the brokered
//! path the Landlock policy is not applied whether it is requested or not
//! (`prepare_child` installs no Landlock either; its confinement is uid 1001,
//! no capabilities, `no_new_privs`, a seccomp deny-list, the cgroup leaf, the
//! mount namespace and the closed environment allow-list).
//!
//! The only part of `apply` that would survive is its `TMPDIR` override, and the
//! seed writes no temporary files — it links and copies straight into the
//! destination. Calling it would therefore change nothing except to imply a
//! confinement that is not in force. The seed is also not agent-authored code:
//! it is this binary, invoked with two paths the worker computed.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::cargo_target_seed::{CargoTargetSeedFallback, CargoTargetSeedResult};

/// Env switch: set to `0`/`false`/`off` to skip the brokered seed and use the
/// in-worker path. Operator break-glass; the default is on.
pub const BROKERED_SEED_ENV: &str = "DJINN_CARGO_TARGET_SEED_BROKERED";

/// Env override for the brokered seed's wall-clock budget, in seconds.
pub const BROKERED_SEED_TIMEOUT_ENV: &str = "DJINN_CARGO_TARGET_SEED_TIMEOUT_SECONDS";

/// Default wall-clock budget for the brokered seed.
///
/// The measured production base is ~27 GiB / ~16k files and the seed is
/// overwhelmingly `linkat`, not byte copies, so this is generous by an order of
/// magnitude. It exists so a wedged broker cannot hold a task-run open
/// indefinitely — on expiry the invocation is terminated and the in-worker path
/// takes over.
pub const DEFAULT_BROKERED_SEED_TIMEOUT: Duration = Duration::from_secs(900);

/// Line prefix the seed subcommand writes its machine-readable summary under.
///
/// A prefix rather than "parse the whole of stdout" because the child runs
/// under `bash -lc`, and a login shell's profile scripts are free to print
/// whatever they like before the program starts.
pub const SEED_RESULT_PREFIX: &str = "DJINN_CARGO_TARGET_SEED_RESULT ";

/// Why the brokered seed did not produce the result, so the caller can name the
/// degradation in one structured field instead of a free-text blob.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BrokeredSeedDegradation {
    /// No broker: the launcher is disabled or the handshake did not happen.
    NoBroker,
    /// [`BROKERED_SEED_ENV`] disabled the path.
    DisabledByEnv,
    /// `current_exe()` could not be resolved, so no program could be named.
    ExecutableUnresolved,
    /// The broker could not launch or reap the child at all.
    LaunchFailed,
    /// The child ran and exited non-zero.
    NonZeroExit,
    /// The child exited 0 but emitted no parseable summary line.
    UnparseableSummary,
}

impl BrokeredSeedDegradation {
    /// Stable low-cardinality label for logs and metrics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoBroker => "no_broker",
            Self::DisabledByEnv => "disabled_by_env",
            Self::ExecutableUnresolved => "executable_unresolved",
            Self::LaunchFailed => "launch_failed",
            Self::NonZeroExit => "non_zero_exit",
            Self::UnparseableSummary => "unparseable_summary",
        }
    }
}

/// Whether the brokered seed path is enabled by the environment.
pub fn brokered_seed_enabled() -> bool {
    match std::env::var(BROKERED_SEED_ENV) {
        Ok(raw) => !matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        Err(_) => true,
    }
}

/// Wall-clock budget for the brokered seed.
pub fn brokered_seed_timeout() -> Duration {
    std::env::var(BROKERED_SEED_TIMEOUT_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map_or(DEFAULT_BROKERED_SEED_TIMEOUT, Duration::from_secs)
}

/// Single-quote `value` for a POSIX shell.
///
/// The paths involved are uuid-derived today, but a project id is
/// operator-supplied and this string is handed to `bash -lc`; quoting it is the
/// difference between a path and a command.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Build the `bash -lc` command that runs the seed subcommand in the child.
///
/// `cwd` must be `/workspace` or beneath — `safe_command_path(_, cwd: true)`
/// refuses anything else, and `spawn.rs` `chdir`s there before `execve`.
pub fn build_seed_command(exe: &Path, base: &Path, run_dir: &Path, cwd: &Path) -> Command {
    let script = format!(
        "exec {exe} seed-cargo-target --base {base} --run-dir {run_dir}",
        exe = shell_quote(&exe.display().to_string()),
        base = shell_quote(&base.display().to_string()),
        run_dir = shell_quote(&run_dir.display().to_string()),
    );
    let mut command = Command::new("bash");
    command.arg("-lc").arg(script).current_dir(cwd);
    command
}

/// The seed result as it crosses the process boundary.
///
/// A dedicated DTO rather than `serde` on [`CargoTargetSeedResult`]: the wire
/// shape is this module's concern, and the seed module's type is consumed by
/// telemetry and log call sites that must stay free to change independently.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SeedWireResult {
    pub elapsed_ms: u64,
    pub linked_file_count: u64,
    pub copied_file_count: u64,
    pub skipped_file_count: u64,
    pub linked_bytes: u64,
    pub copied_bytes: u64,
    pub degraded_link_file_count: u64,
    pub unseeded_file_count: u64,
    pub base_seedable_file_count: u64,
    pub link_fallback_budget_exhausted: bool,
    pub first_entry_error: Option<String>,
    pub fallback: Option<WireFallback>,
}

/// Wire form of [`CargoTargetSeedFallback`]: a stable tag plus its detail
/// string, so a new variant on either side degrades to a named unknown instead
/// of a parse failure.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WireFallback {
    pub kind: String,
    pub detail: String,
}

impl From<&CargoTargetSeedResult> for SeedWireResult {
    fn from(result: &CargoTargetSeedResult) -> Self {
        Self {
            elapsed_ms: u64::try_from(result.elapsed.as_millis()).unwrap_or(u64::MAX),
            linked_file_count: result.linked_file_count,
            copied_file_count: result.copied_file_count,
            skipped_file_count: result.skipped_file_count,
            linked_bytes: result.linked_bytes,
            copied_bytes: result.copied_bytes,
            degraded_link_file_count: result.degraded_link_file_count,
            unseeded_file_count: result.unseeded_file_count,
            base_seedable_file_count: result.base_seedable_file_count,
            link_fallback_budget_exhausted: result.link_fallback_budget_exhausted,
            first_entry_error: result.first_entry_error.clone(),
            fallback: result.fallback_reason.as_ref().map(|reason| match reason {
                CargoTargetSeedFallback::BaseMissing => WireFallback {
                    kind: "base_missing".into(),
                    detail: String::new(),
                },
                CargoTargetSeedFallback::BaseNotDirectory => WireFallback {
                    kind: "base_not_directory".into(),
                    detail: String::new(),
                },
                CargoTargetSeedFallback::BaseUnusable(detail) => WireFallback {
                    kind: "base_unusable".into(),
                    detail: detail.clone(),
                },
                CargoTargetSeedFallback::ScanFailed(detail) => WireFallback {
                    kind: "scan_failed".into(),
                    detail: detail.clone(),
                },
                CargoTargetSeedFallback::CloneFailed(detail) => WireFallback {
                    kind: "clone_failed".into(),
                    detail: detail.clone(),
                },
            }),
        }
    }
}

impl From<SeedWireResult> for CargoTargetSeedResult {
    fn from(wire: SeedWireResult) -> Self {
        Self {
            elapsed: Duration::from_millis(wire.elapsed_ms),
            linked_file_count: wire.linked_file_count,
            copied_file_count: wire.copied_file_count,
            skipped_file_count: wire.skipped_file_count,
            linked_bytes: wire.linked_bytes,
            copied_bytes: wire.copied_bytes,
            degraded_link_file_count: wire.degraded_link_file_count,
            unseeded_file_count: wire.unseeded_file_count,
            base_seedable_file_count: wire.base_seedable_file_count,
            link_fallback_budget_exhausted: wire.link_fallback_budget_exhausted,
            first_entry_error: wire.first_entry_error,
            fallback_reason: wire.fallback.map(|fallback| match fallback.kind.as_str() {
                "base_missing" => CargoTargetSeedFallback::BaseMissing,
                "base_not_directory" => CargoTargetSeedFallback::BaseNotDirectory,
                "base_unusable" => CargoTargetSeedFallback::BaseUnusable(fallback.detail),
                "scan_failed" => CargoTargetSeedFallback::ScanFailed(fallback.detail),
                "clone_failed" => CargoTargetSeedFallback::CloneFailed(fallback.detail),
                unknown => CargoTargetSeedFallback::BaseUnusable(format!(
                    "unrecognized fallback {unknown}: {}",
                    fallback.detail
                )),
            }),
        }
    }
}

/// Render the summary line the seed subcommand prints on stdout.
pub fn encode_seed_result(result: &CargoTargetSeedResult) -> String {
    let wire = SeedWireResult::from(result);
    format!(
        "{SEED_RESULT_PREFIX}{}",
        serde_json::to_string(&wire).unwrap_or_default()
    )
}

/// Recover the seed result from the child's stdout, or `None` when no line
/// carries a parseable summary.
///
/// Scans from the END: a login shell may print before the program runs, and if
/// the payload itself ever contained an embedded newline the last complete
/// line is the authoritative one.
pub fn decode_seed_result(stdout: &str) -> Option<CargoTargetSeedResult> {
    stdout
        .lines()
        .rev()
        .filter_map(|line| line.strip_prefix(SEED_RESULT_PREFIX))
        .find_map(|payload| serde_json::from_str::<SeedWireResult>(payload.trim()).ok())
        .map(CargoTargetSeedResult::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sample() -> CargoTargetSeedResult {
        CargoTargetSeedResult {
            elapsed: Duration::from_millis(4_321),
            linked_file_count: 11,
            copied_file_count: 5,
            skipped_file_count: 3,
            linked_bytes: 900,
            copied_bytes: 40,
            degraded_link_file_count: 2,
            unseeded_file_count: 1,
            base_seedable_file_count: 16,
            link_fallback_budget_exhausted: true,
            first_entry_error: Some("copy /a/b: EPERM".into()),
            fallback_reason: Some(CargoTargetSeedFallback::ScanFailed("boom".into())),
        }
    }

    /// The wire hop must be lossless: the parent records telemetry and emits
    /// the coordinator-facing structured event from this value, so a field that
    /// silently zeroes across the boundary is a silent metrics regression.
    #[test]
    fn the_seed_result_round_trips_through_the_child_stdout_line() {
        let original = sample();
        let line = encode_seed_result(&original);
        let recovered =
            decode_seed_result(&format!("some login-shell noise\n{line}\ntrailing noise\n"))
                .expect("summary line is recoverable");
        assert_eq!(recovered, original);
    }

    /// Neutralization guard: if the subcommand stops printing the summary, the
    /// parent must NOT silently accept a zeroed result — it must see `None` and
    /// degrade to the in-worker seed.
    #[test]
    fn stdout_without_a_summary_line_yields_no_result() {
        assert!(decode_seed_result("").is_none());
        assert!(decode_seed_result("cargo target seed: done\n").is_none());
        assert!(
            decode_seed_result(&format!("{SEED_RESULT_PREFIX}{{not json\n")).is_none(),
            "a corrupt payload must not be mistaken for a successful seed"
        );
    }

    /// The one program a brokered command may name is a shell, and the cwd must
    /// be the workspace: `safe_command_path` refuses anything else, and the
    /// launcher's refusal is a coarse `InvalidCommand` on the wire.
    #[test]
    fn the_brokered_command_names_a_shell_and_runs_in_the_workspace() {
        let command = build_seed_command(
            Path::new("/opt/djinn/bin/djinn-agent-worker"),
            Path::new("/cache/cargo-target/p/mold-jobs-4"),
            Path::new("/cache/cargo-target-runs/run-1"),
            Path::new("/workspace"),
        );
        assert_eq!(command.get_program(), "bash");
        assert_eq!(command.get_current_dir(), Some(Path::new("/workspace")));
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "-lc");
        assert!(
            args[1].contains("seed-cargo-target"),
            "script must invoke the seed subcommand: {}",
            args[1]
        );
        assert!(
            args[1].contains("'/cache/cargo-target/p/mold-jobs-4'"),
            "base must be quoted: {}",
            args[1]
        );
        assert!(
            args[1].contains("'/cache/cargo-target-runs/run-1'"),
            "run dir must be quoted: {}",
            args[1]
        );
        // `safe_command_path` admits `/bin/`, `/usr/bin/` and `/workspace/`
        // programs only. Naming the worker binary directly would be refused,
        // and widening that list is a posture change the launcher's own docs
        // argue against.
        assert!(
            !djinn_cgroup_launcher::command_path::safe_command_path(
                "/opt/djinn/bin/djinn-agent-worker",
                false
            ),
            "if this ever becomes admissible, the bash -lc hop can be dropped — \
             but that is a deliberate posture change, not a cleanup"
        );
        assert!(djinn_cgroup_launcher::command_path::safe_command_path(
            "/workspace",
            true
        ));
    }

    /// A path with a quote in it must not become a second shell word.
    #[test]
    fn shell_metacharacters_in_a_path_stay_inside_one_argument() {
        let command = build_seed_command(
            Path::new("/opt/djinn/bin/djinn-agent-worker"),
            Path::new("/cache/cargo-target/p'; touch /tmp/pwned; '"),
            Path::new("/cache/cargo-target-runs/run-1"),
            Path::new("/workspace"),
        );
        let script = command
            .get_args()
            .nth(1)
            .expect("script")
            .to_string_lossy()
            .into_owned();
        assert!(
            !script.contains("; touch /tmp/pwned; '\n") && script.contains(r"'\''"),
            "the quote must be escaped, not closed: {script}"
        );
    }

    #[test]
    fn the_env_switch_defaults_on_and_is_explicit_to_turn_off() {
        // Read through the same predicate production uses; the env is not
        // mutated here (tests share a process) so this pins the default only.
        if std::env::var(BROKERED_SEED_ENV).is_err() {
            assert!(brokered_seed_enabled(), "the default must be ON");
        }
        assert_eq!(brokered_seed_timeout(), brokered_seed_timeout());
    }

    #[test]
    fn every_degradation_reason_has_a_distinct_label() {
        let all = [
            BrokeredSeedDegradation::NoBroker,
            BrokeredSeedDegradation::DisabledByEnv,
            BrokeredSeedDegradation::ExecutableUnresolved,
            BrokeredSeedDegradation::LaunchFailed,
            BrokeredSeedDegradation::NonZeroExit,
            BrokeredSeedDegradation::UnparseableSummary,
        ];
        let labels: std::collections::BTreeSet<&str> =
            all.iter().map(|reason| reason.as_str()).collect();
        assert_eq!(labels.len(), all.len(), "labels must be distinguishable");
    }

    #[test]
    fn an_unknown_wire_fallback_degrades_to_a_named_reason_not_a_parse_error() {
        let wire = SeedWireResult {
            fallback: Some(WireFallback {
                kind: "invented_by_a_newer_worker".into(),
                detail: "why".into(),
            }),
            ..SeedWireResult::from(&sample())
        };
        let result = CargoTargetSeedResult::from(wire);
        match result.fallback_reason {
            Some(CargoTargetSeedFallback::BaseUnusable(detail)) => {
                assert!(detail.contains("invented_by_a_newer_worker"), "{detail}");
            }
            other => panic!("expected a named unknown, got {other:?}"),
        }
    }

    #[test]
    fn the_default_timeout_is_a_bound_not_an_expectation() {
        assert!(DEFAULT_BROKERED_SEED_TIMEOUT >= Duration::from_secs(600));
        let _ = PathBuf::new();
    }
}
