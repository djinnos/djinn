//! One-shot operator admin commands.
//!
//! These are short-circuit modes of the server binary (like `--migrate-only`):
//! they open the database, perform one operation, render a result, and exit
//! before any actor, listener, or background task starts. The rendered `String`
//! is returned to `main`, which prints it — this module performs no I/O to
//! stdout/stderr itself so the coordinator lint budget stays clean.
//!
//! The `epoch` subtree is the operator surface for the **invocation-lease
//! authority**: the durable arming switch and reference cap for the
//! per-invocation cgroup CPU lease. It drives
//! [`InvocationLeaseControl`], which composes the durable primitives; nothing
//! here re-implements fencing.
//!
//! `seed` is the one step that creates the durable row rather than mutating it.
//! Startup deliberately never re-creates an absent row — an absent authority is
//! a disarmed one, and removing the row is the documented remediation for a
//! wedged authority — so restoring it is an explicit operator action rather than
//! an implicit deploy-time side effect.
//!
//! # What the Kueue cutover removed here (S3b)
//!
//! `epoch advance` and `epoch rollback` are gone. They drove a four-phase ring
//! (`emergency_primary → forward_overlap → invocation_primary →
//! rollback_overlap`) whose only job was to hand admission authority between a v0
//! "emergency" ledger and the v1 invocation authority without ever leaving zero
//! enforcing authorities. The Kueue cutover deleted v0, so there is no handover
//! to sequence and no phase to be in. `arm` and `kill-switch` express everything
//! that is left, in one epoch-fenced write each. See
//! `docs/BUILD_ADMISSION_EPOCH_RUNBOOK.md`.

use std::fmt::Write as _;
use std::sync::Arc;

use djinn_coordinator::invocation_lease_control::{ControlError, InvocationLeaseControl};
use djinn_db::{
    Database, InvocationLeaseAuthorityRepository, InvocationLeaseAuthorityRow, InvocationLeaseMode,
};

/// Top-level operator admin commands.
#[derive(clap::Subcommand, Debug)]
pub enum AdminCommand {
    /// Invocation-lease authority operator commands.
    Epoch {
        #[command(subcommand)]
        action: EpochAction,
    },
}

/// The arming mode, as an operator spells it on the command line.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeArg {
    /// Disarmed: no invocation is leased.
    Off,
    /// Bind and measure, but never lift the reserved quota. NOTE this CLAMPS
    /// every leased build to the unleased quota for its whole life — it is an
    /// observation mode, not a faster one.
    Shadow,
    /// Armed: a bound invocation may lift `cpu.max`.
    Enforce,
}

impl From<ModeArg> for InvocationLeaseMode {
    fn from(value: ModeArg) -> Self {
        match value {
            ModeArg::Off => Self::Off,
            ModeArg::Shadow => Self::Shadow,
            ModeArg::Enforce => Self::Enforce,
        }
    }
}

/// Invocation-lease authority operator actions.
#[derive(clap::Subcommand, Debug)]
pub enum EpochAction {
    /// Print the durable authority row.
    Show,
    /// Create the durable authority row, DISARMED, when it is absent.
    /// Idempotent: an existing row is reported and left untouched.
    Seed,
    /// Set the arming mode. `--cap` is required the first time the authority is
    /// armed and preserved otherwise.
    Arm {
        #[arg(long, value_enum)]
        mode: ModeArg,
        /// Reference cap; defaults to the current cap.
        #[arg(long)]
        cap: Option<i64>,
    },
    /// Change the reference cap, preserving the arming mode. Adopted by running
    /// processes without a restart.
    SetCap {
        #[arg(long)]
        cap: i64,
    },
    /// Urgent disarm: set the mode to `off` in one epoch-fenced write, keeping
    /// the cap so re-arming needs no new number.
    KillSwitch,
}

/// Run an admin command against `db`, returning a rendered result to print.
///
/// A returned `Err` is a genuine failure (stale epoch, invalid configuration,
/// storage error) that should exit non-zero.
pub async fn run_admin_command(db: &Database, command: AdminCommand) -> Result<String, String> {
    let repo = Arc::new(InvocationLeaseAuthorityRepository::new(db.clone()));
    let control = InvocationLeaseControl::new(repo);
    match command {
        AdminCommand::Epoch { action } => run_epoch_action(&control, action).await,
    }
}

async fn run_epoch_action(
    control: &InvocationLeaseControl,
    action: EpochAction,
) -> Result<String, String> {
    match action {
        EpochAction::Show => {
            let row = control.show().await.map_err(|e| e.to_string())?;
            Ok(render_show(row.as_ref()))
        }
        EpochAction::Seed => {
            if let Some(row) = control.show().await.map_err(|e| e.to_string())? {
                return Ok(format!(
                    "seed: already present, left untouched\n{}",
                    render_show(Some(&row))
                ));
            }
            let row = control.seed().await.map_err(|e| e.to_string())?;
            Ok(format!("seed: applied\n{}", render_show(Some(&row))))
        }
        EpochAction::Arm { mode, cap } => {
            let row = require_row(control).await?;
            fold(control.arm(row.epoch, mode.into(), cap).await, "arm")
        }
        EpochAction::SetCap { cap } => {
            let row = require_row(control).await?;
            fold(control.set_cap(row.epoch, cap).await, "set-cap")
        }
        EpochAction::KillSwitch => {
            let row = require_row(control).await?;
            fold(control.kill_switch(row.epoch).await, "kill-switch")
        }
    }
}

async fn require_row(
    control: &InvocationLeaseControl,
) -> Result<InvocationLeaseAuthorityRow, String> {
    control
        .show()
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| ControlError::AuthorityAbsent.to_string())
}

fn fold(
    result: Result<InvocationLeaseAuthorityRow, ControlError>,
    step: &str,
) -> Result<String, String> {
    match result {
        Ok(row) => Ok(format!("{step}: applied\n{}", render_show(Some(&row)))),
        Err(err) => Err(format!("{step}: {err}")),
    }
}

fn render_show(row: Option<&InvocationLeaseAuthorityRow>) -> String {
    let Some(row) = row else {
        return "invocation lease authority: <absent> (disarmed; run `seed` to create it)"
            .to_string();
    };
    let mut out = String::new();
    let _ = writeln!(out, "mode                  {:?}", row.mode);
    let _ = writeln!(
        out,
        "cap                   {}",
        row.cap
            .map_or_else(|| "<unset>".to_string(), |c| c.to_string())
    );
    let _ = writeln!(out, "epoch                 {}", row.epoch);
    let _ = write!(out, "updated_at            {}", row.updated_at);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **AC4: every surviving subcommand executes against a real database.**
    ///
    /// Not "the enum still has five variants" — each one is dispatched through
    /// `run_epoch_action`, the exact path `main` takes, and its effect on the
    /// durable row is asserted. A subcommand that still resolved to a deleted
    /// relation would fail here rather than at 3am.
    #[tokio::test]
    async fn every_surviving_subcommand_runs_against_a_real_database() {
        let db = Database::open_in_memory().expect("test database");
        let repo = Arc::new(InvocationLeaseAuthorityRepository::new(db));
        repo.read().await.expect("initialize fixture");
        repo.delete_for_test().await.expect("remove row");
        let control = InvocationLeaseControl::new(Arc::clone(&repo));

        // `show` against an absent authority names the state and the remedy.
        let rendered = run_epoch_action(&control, EpochAction::Show)
            .await
            .expect("show never fails on an absent row");
        assert!(rendered.contains("<absent>"), "{rendered}");
        assert!(rendered.contains("seed"), "{rendered}");

        // `seed` creates it, DISARMED.
        let rendered = run_epoch_action(&control, EpochAction::Seed)
            .await
            .expect("seed applies");
        assert!(rendered.starts_with("seed: applied"), "{rendered}");
        let seeded = repo.read().await.expect("read").expect("seeded row");
        assert_eq!(seeded.mode, InvocationLeaseMode::Off);

        // `seed` again is idempotent and never destructive.
        let rendered = run_epoch_action(&control, EpochAction::Seed)
            .await
            .expect("repeat seed reports the existing row");
        assert!(rendered.starts_with("seed: already present"), "{rendered}");
        assert_eq!(repo.read().await.expect("read").expect("row"), seeded);

        // `arm --mode shadow --cap 3`.
        let rendered = run_epoch_action(
            &control,
            EpochAction::Arm {
                mode: ModeArg::Shadow,
                cap: Some(3),
            },
        )
        .await
        .expect("arm shadow");
        assert!(rendered.starts_with("arm: applied"), "{rendered}");
        assert_eq!(
            repo.read().await.expect("read").expect("row").mode,
            InvocationLeaseMode::Shadow
        );

        // `arm --mode enforce` inherits the stored cap.
        run_epoch_action(
            &control,
            EpochAction::Arm {
                mode: ModeArg::Enforce,
                cap: None,
            },
        )
        .await
        .expect("arm enforce");
        let armed = repo.read().await.expect("read").expect("row");
        assert_eq!(armed.mode, InvocationLeaseMode::Enforce);
        assert_eq!(armed.cap, Some(3));

        // `set-cap` preserves the mode.
        let rendered = run_epoch_action(&control, EpochAction::SetCap { cap: 12 })
            .await
            .expect("set-cap");
        assert!(rendered.starts_with("set-cap: applied"), "{rendered}");
        let recapped = repo.read().await.expect("read").expect("row");
        assert_eq!(recapped.cap, Some(12));
        assert_eq!(
            recapped.mode,
            InvocationLeaseMode::Enforce,
            "a cap change must never disarm the authority"
        );

        // `kill-switch` disarms and keeps the cap.
        let rendered = run_epoch_action(&control, EpochAction::KillSwitch)
            .await
            .expect("kill-switch");
        assert!(rendered.starts_with("kill-switch: applied"), "{rendered}");
        let killed = repo.read().await.expect("read").expect("row");
        assert_eq!(killed.mode, InvocationLeaseMode::Off);
        assert_eq!(killed.cap, Some(12));

        // And `show` renders the surviving fields, none of them retired.
        let rendered = run_epoch_action(&control, EpochAction::Show)
            .await
            .expect("show");
        for field in ["mode", "cap", "epoch", "updated_at"] {
            assert!(rendered.contains(field), "{field} missing from {rendered}");
        }
        for retired in ["phase", "v0_mode", "v1_mode", "ack"] {
            assert!(
                !rendered.contains(retired),
                "`{retired}` belongs to the retired v0↔v1 handoff and must not be \
                 rendered: {rendered}"
            );
        }
    }

    /// An operator error is a non-zero exit with an actionable message, not a
    /// silently-applied write.
    #[tokio::test]
    async fn an_invalid_cap_is_refused_and_leaves_the_row_untouched() {
        let db = Database::open_in_memory().expect("test database");
        let repo = Arc::new(InvocationLeaseAuthorityRepository::new(db));
        let before = repo.read().await.expect("read").expect("seeded row");
        let control = InvocationLeaseControl::new(Arc::clone(&repo));

        let error = run_epoch_action(&control, EpochAction::SetCap { cap: 0 })
            .await
            .expect_err("a zero cap must be refused");
        assert!(error.contains("out of range"), "{error}");
        assert_eq!(
            repo.read().await.expect("read").expect("row"),
            before,
            "a refused command must not mutate the durable row"
        );
    }
}
