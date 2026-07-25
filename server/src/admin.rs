//! One-shot operator admin commands.
//!
//! These are short-circuit modes of the server binary (like `--migrate-only`):
//! they open the database, perform one operation, render a result, and exit
//! before any actor, listener, or background task starts. The rendered `String`
//! is returned to `main`, which prints it — this module performs no I/O to
//! stdout/stderr itself so the coordinator lint budget stays clean.
//!
//! The `epoch` subtree drives the Build Leases v1 admission-epoch rollout through
//! [`AdmissionTransitionExecutor`], the safe-ordering executor. Each `advance` /
//! `rollback` / `kill-switch` invocation performs exactly one epoch-fenced step
//! from the current durable state and reports what it did or what it is waiting
//! on, so an operator (or the runbook) re-runs the command as controller and
//! generation acknowledgements land.
//!
//! `seed` is the one step that creates the durable row rather than transitioning
//! it. Startup deliberately never re-creates an absent row — mapping absence to
//! the configured standalone v0 mode is the documented remediation for a wedged
//! handoff — so restoring the rollout after that remediation is an explicit
//! operator action rather than an implicit deploy-time side effect.

use std::fmt::Write as _;
use std::sync::Arc;

use djinn_coordinator::build_admission_transition::{AdmissionTransitionExecutor, TransitionError};
use djinn_db::{
    AdmissionHandoffPhase, AdmissionHandoffRepository, AdmissionHandoffRow, Database, V0Mode,
    V1Mode,
};

/// Top-level operator admin commands.
#[derive(clap::Subcommand, Debug)]
pub enum AdminCommand {
    /// Admission-epoch operator commands (Build Leases v1 rollout).
    Epoch {
        #[command(subcommand)]
        action: EpochAction,
    },
}

/// Admission-epoch operator actions.
#[derive(clap::Subcommand, Debug)]
pub enum EpochAction {
    /// Print the durable epoch row and its derived handoff state.
    Show,
    /// Create the durable epoch row at its v0-enforcing baseline when it is
    /// absent. Idempotent: an existing row is reported and left untouched.
    Seed,
    /// Perform the next safe FORWARD step: arm shadow → arm overlap → enter
    /// overlap → commit invocation-primary → observe v0.
    Advance {
        /// Reference cap for an arming step; defaults to the current cap.
        #[arg(long)]
        cap: Option<i64>,
        /// Live generation keys that must acknowledge before invocation-primary.
        #[arg(long = "generation")]
        generations: Vec<String>,
    },
    /// Change the reference cap (epoch-fenced; serializes with the handoff edge).
    SetCap {
        #[arg(long)]
        cap: i64,
    },
    /// Perform the next safe ROLLBACK step from invocation-primary.
    Rollback {
        /// Same-or-lower cap for the rollback arm; defaults to the current cap.
        #[arg(long)]
        cap: Option<i64>,
    },
    /// Urgent rollback from invocation-primary — the same safe ordering as
    /// `rollback`, initiated while v0 is still enforcing from the overlap.
    KillSwitch {
        /// Same-or-lower cap for the rollback arm; defaults to the current cap.
        #[arg(long)]
        cap: Option<i64>,
    },
}

/// Run an admin command against `db`, returning a rendered result to print.
///
/// A returned `Err` is a genuine failure (invalid transition, invalid config,
/// storage error) that should exit non-zero. Expected "waiting" and "halted"
/// outcomes are folded into the `Ok` rendering so an operator polling the command
/// sees them without a non-zero exit.
pub async fn run_admin_command(db: &Database, command: AdminCommand) -> Result<String, String> {
    let repo = Arc::new(AdmissionHandoffRepository::new(db.clone()));
    let exec = AdmissionTransitionExecutor::new(repo);
    match command {
        AdminCommand::Epoch { action } => run_epoch_action(&exec, action).await,
    }
}

async fn run_epoch_action(
    exec: &AdmissionTransitionExecutor,
    action: EpochAction,
) -> Result<String, String> {
    match action {
        EpochAction::Show => {
            let row = exec.show().await.map_err(|e| e.to_string())?;
            Ok(render_show(row.as_ref()))
        }
        EpochAction::Seed => {
            if let Some(row) = exec.show().await.map_err(|e| e.to_string())? {
                return Ok(format!(
                    "seed: already present, left untouched\n{}",
                    render_show(Some(&row))
                ));
            }
            let row = exec.seed().await.map_err(|e| e.to_string())?;
            Ok(format!("seed: applied\n{}", render_show(Some(&row))))
        }
        EpochAction::SetCap { cap } => {
            let row = require_row(exec).await?;
            fold(exec.set_cap(row.epoch, cap).await, "set-cap")
        }
        EpochAction::Advance { cap, generations } => advance_forward(exec, cap, &generations).await,
        EpochAction::Rollback { cap } | EpochAction::KillSwitch { cap } => {
            advance_rollback(exec, cap).await
        }
    }
}

async fn require_row(exec: &AdmissionTransitionExecutor) -> Result<AdmissionHandoffRow, String> {
    exec.show()
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "admission handoff row is absent".to_string())
}

/// Drive the next forward step from the current durable state.
async fn advance_forward(
    exec: &AdmissionTransitionExecutor,
    cap: Option<i64>,
    generations: &[String],
) -> Result<String, String> {
    let row = require_row(exec).await?;
    let epoch = row.epoch;
    match (row.phase, row.v0_mode, row.v1_mode) {
        (AdmissionHandoffPhase::EmergencyPrimary, _, V1Mode::Off) => {
            let cap = resolve_cap(cap, row.cap)?;
            fold(exec.arm_shadow(epoch, cap).await, "arm-shadow")
        }
        (AdmissionHandoffPhase::EmergencyPrimary, _, V1Mode::Shadow) => {
            let cap = resolve_cap(cap, row.cap)?;
            fold(exec.arm_overlap(epoch, cap).await, "arm-overlap")
        }
        (AdmissionHandoffPhase::EmergencyPrimary, _, V1Mode::Enforce) => fold(
            exec.enter_forward_overlap(epoch).await,
            "enter-forward-overlap",
        ),
        (AdmissionHandoffPhase::ForwardOverlap, _, _) => fold(
            exec.commit_invocation_primary(epoch, generations).await,
            "commit-invocation-primary",
        ),
        (AdmissionHandoffPhase::InvocationPrimary, V0Mode::Enforce, _) => {
            fold(exec.observe_v0(epoch).await, "observe-v0")
        }
        (AdmissionHandoffPhase::InvocationPrimary, _, _) => {
            Ok("forward cutover complete: invocation-primary with v0 observing".to_string())
        }
        (phase, _, _) => Err(format!("no forward step is defined from phase {phase:?}")),
    }
}

/// Drive the next rollback step from the current durable state.
async fn advance_rollback(
    exec: &AdmissionTransitionExecutor,
    cap: Option<i64>,
) -> Result<String, String> {
    let row = require_row(exec).await?;
    let epoch = row.epoch;
    match row.phase {
        AdmissionHandoffPhase::InvocationPrimary => {
            let both_enforce = row.v0_mode == V0Mode::Enforce && row.v1_mode == V1Mode::Enforce;
            if both_enforce {
                fold(
                    exec.enter_rollback_overlap(epoch).await,
                    "enter-rollback-overlap",
                )
            } else {
                let cap = resolve_cap(cap, row.cap)?;
                fold(exec.arm_rollback(epoch, cap).await, "arm-rollback")
            }
        }
        AdmissionHandoffPhase::RollbackOverlap => {
            fold(exec.complete_rollback(epoch).await, "complete-rollback")
        }
        AdmissionHandoffPhase::EmergencyPrimary if row.v1_mode == V1Mode::Off => {
            Ok("rollback complete: emergency-primary baseline (v0 enforce, v1 off)".to_string())
        }
        phase => Err(format!("no rollback step is defined from phase {phase:?}")),
    }
}

/// Use the requested cap, or fall back to the current durable cap; error when
/// neither is available (an arming step must have a concrete cap).
fn resolve_cap(requested: Option<i64>, current: Option<i64>) -> Result<i64, String> {
    requested
        .or(current)
        .ok_or_else(|| "this step needs a cap; pass --cap N".to_string())
}

/// Fold a step result: success and the expected waiting/halted outcomes render
/// to `Ok`; genuine failures render to `Err`.
fn fold(
    result: Result<AdmissionHandoffRow, TransitionError>,
    step: &str,
) -> Result<String, String> {
    match result {
        Ok(row) => Ok(format!("{step}: applied\n{}", render_show(Some(&row)))),
        Err(err @ TransitionError::AwaitingEmergencyAck { .. })
        | Err(err @ TransitionError::AwaitingGenerationAcks { .. })
        | Err(err @ TransitionError::HaltedV0Unconfirmed { .. }) => {
            Ok(format!("{step}: waiting — {err}"))
        }
        Err(err) => Err(format!("{step}: {err}")),
    }
}

fn render_show(row: Option<&AdmissionHandoffRow>) -> String {
    let Some(row) = row else {
        return "admission handoff row: <absent>".to_string();
    };
    let mut out = String::new();
    let _ = writeln!(out, "phase                 {:?}", row.phase);
    let _ = writeln!(out, "epoch                 {}", row.epoch);
    let _ = writeln!(out, "v0_mode               {:?}", row.v0_mode);
    let _ = writeln!(out, "v1_mode               {:?}", row.v1_mode);
    let _ = writeln!(
        out,
        "cap                   {}",
        row.cap
            .map_or_else(|| "<unset>".to_string(), |c| c.to_string())
    );
    let _ = writeln!(
        out,
        "emergency_ack_epoch   {}",
        row.emergency_ack_epoch
            .map_or_else(|| "<none>".to_string(), |e| e.to_string())
    );
    let _ = write!(
        out,
        "invocation_ack_epoch  {}",
        row.invocation_ack_epoch
            .map_or_else(|| "<none>".to_string(), |e| e.to_string())
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `epoch seed` is the operator surface that restores a deployment whose row
    /// was deleted, and it must land on a complete (already acknowledged) v0
    /// baseline so the next `advance` is not blocked waiting for an ack that
    /// only a healthy coordinator leader can write.
    #[tokio::test]
    async fn seed_creates_an_acknowledged_baseline_and_is_idempotent() {
        let db = Database::open_in_memory().expect("test database");
        let repo = Arc::new(AdmissionHandoffRepository::new(db));
        repo.read().await.expect("initialize fixture");
        repo.delete_for_test().await.expect("remove row");

        let rendered = run_admin_command_with(&repo, EpochAction::Seed)
            .await
            .expect("seed applies");
        assert!(rendered.starts_with("seed: applied"), "{rendered}");
        let seeded = repo.read().await.expect("read").expect("seeded row");
        assert_eq!(seeded.phase, AdmissionHandoffPhase::EmergencyPrimary);
        assert_eq!(seeded.v0_mode, V0Mode::Enforce);
        assert_eq!(seeded.v1_mode, V1Mode::Off);
        assert_eq!(seeded.emergency_ack_epoch, Some(seeded.epoch));

        let rendered = run_admin_command_with(&repo, EpochAction::Seed)
            .await
            .expect("repeat seed reports the existing row");
        assert!(rendered.starts_with("seed: already present"), "{rendered}");
        assert_eq!(repo.read().await.expect("read").expect("row"), seeded);

        // The acknowledged baseline is immediately armable: the shadow step no
        // longer has to wait for a coordinator tick to complete the epoch.
        let rendered = run_admin_command_with(
            &repo,
            EpochAction::Advance {
                cap: Some(3),
                generations: Vec::new(),
            },
        )
        .await
        .expect("arm shadow");
        assert!(rendered.starts_with("arm-shadow: applied"), "{rendered}");
    }

    async fn run_admin_command_with(
        repo: &Arc<AdmissionHandoffRepository>,
        action: EpochAction,
    ) -> Result<String, String> {
        run_epoch_action(&AdmissionTransitionExecutor::new(repo.clone()), action).await
    }
}
