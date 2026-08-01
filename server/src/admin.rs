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
    BuildLeaseConsumerKind, BuildLeaseKey, BuildLeaseRepository, BuildLeaseRow, Database,
    InvocationLeaseAuthorityRepository, InvocationLeaseAuthorityRow, InvocationLeaseMode,
    LauncherAuthorityModeRepository, SetLauncherAuthorityModeResult,
};
use djinn_launcher_protocol::LauncherAuthorityProtocol;

/// Top-level operator admin commands.
#[derive(clap::Subcommand, Debug)]
pub enum AdminCommand {
    /// Invocation-lease authority operator commands.
    Epoch {
        #[command(subcommand)]
        action: EpochAction,
    },
    /// Build-lease ledger operator commands.
    BuildLease {
        #[command(subcommand)]
        action: BuildLeaseAction,
    },
    /// Durable launcher quota-authority controls.
    LauncherAuthority {
        #[command(subcommand)]
        action: LauncherAuthorityAction,
    },
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum LauncherAuthorityModeArg {
    LeafV1,
    ResizeV2,
}

impl From<LauncherAuthorityModeArg> for LauncherAuthorityProtocol {
    fn from(value: LauncherAuthorityModeArg) -> Self {
        match value {
            LauncherAuthorityModeArg::LeafV1 => Self::LeafV1,
            LauncherAuthorityModeArg::ResizeV2 => Self::ResizeV2,
        }
    }
}

#[derive(clap::Subcommand, Debug)]
pub enum LauncherAuthorityAction {
    /// Report the durable mode and CAS epoch.
    Show,
    /// Change authority only behind an empty drain and the expected epoch.
    Set {
        #[arg(value_enum)]
        mode: LauncherAuthorityModeArg,
        #[arg(long)]
        expected_epoch: i64,
    },
}

/// Build-lease ledger operator actions.
///
/// `deploy/kueue/preflight.sh` refuses the authority cutover (exit 30) while
/// any nonterminal `build_leases` row exists, and its refusal message says
/// stale rows "must be explicitly cleared". Until this subtree existed nothing
/// could clear them — no CLI, no MCP tool, no admin route — so the only way
/// past that gate was a hand-written SQL `UPDATE` against production, which
/// happened twice on 2026-07-30.
///
/// This is the last resort, not the first. The reclaimer retires an ownerless
/// lease on its own within one settle window; `clear` exists for the case where
/// an operator has looked and decided, and it stamps `operator_cleared` so the
/// ledger never confuses a human decision with a proof.
#[derive(clap::Subcommand, Debug)]
pub enum BuildLeaseAction {
    /// List every nonterminal row — exactly the population the cutover
    /// preflight counts.
    List,
    /// Retire named nonterminal rows.
    ///
    /// Each `--lease` is `<consumer_kind>:<consumer_id>`. Naming them
    /// individually is deliberate: an operator who wants everything runs
    /// `list` first and passes what they read, so a clear is always against a
    /// population they actually saw.
    Clear {
        /// Repeatable. `task_invocation:019fba8a-…`.
        #[arg(long = "lease", required = true)]
        leases: Vec<String>,
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
        AdminCommand::BuildLease { action } => {
            run_build_lease_action(&BuildLeaseRepository::new(db.clone()), action).await
        }
        AdminCommand::LauncherAuthority { action } => {
            run_launcher_authority_action(&LauncherAuthorityModeRepository::new(db.clone()), action)
                .await
        }
    }
}

async fn run_launcher_authority_action(
    repository: &LauncherAuthorityModeRepository,
    action: LauncherAuthorityAction,
) -> Result<String, String> {
    match action {
        LauncherAuthorityAction::Show => repository
            .read()
            .await
            .map_err(|error| error.to_string())?
            .map(|row| format!("mode={}\nepoch={}", row.mode.as_wire(), row.epoch))
            .ok_or_else(|| "launcher authority mode is uninitialized".to_owned()),
        LauncherAuthorityAction::Set {
            mode,
            expected_epoch,
        } => {
            let observed = repository
                .drain_census()
                .await
                .map_err(|error| format!("launcher authority drain census failed: {error}"))?;
            if !observed.is_drained() {
                return Err(format!("launcher authority drain refused: {observed:?}"));
            }
            match repository.set_mode(expected_epoch, mode.into()).await {
                SetLauncherAuthorityModeResult::Flipped { row, .. } => Ok(format!(
                    "launcher authority set: mode={} epoch={}",
                    row.mode.as_wire(),
                    row.epoch
                )),
                SetLauncherAuthorityModeResult::Unchanged { row, .. } => Err(format!(
                    "launcher authority unchanged: mode={} epoch={}",
                    row.mode.as_wire(),
                    row.epoch
                )),
                SetLauncherAuthorityModeResult::DrainNotEmpty { drain, .. } => {
                    Err(format!("launcher authority drain refused: {drain:?}"))
                }
                SetLauncherAuthorityModeResult::EpochConflict { row } => Err(format!(
                    "launcher authority epoch conflict: expected {expected_epoch}, current {}",
                    row.epoch
                )),
                SetLauncherAuthorityModeResult::Uninitialized => {
                    Err("launcher authority mode is uninitialized".to_owned())
                }
                SetLauncherAuthorityModeResult::Unavailable => {
                    Err("launcher authority mode is unavailable".to_owned())
                }
            }
        }
    }
}

/// Parse `<consumer_kind>:<consumer_id>` into a ledger key.
///
/// The kind is validated against the closed enum rather than passed through, so
/// a typo is refused before it can silently match nothing and be reported as a
/// successful clear of zero rows.
fn parse_lease_key(raw: &str) -> Result<BuildLeaseKey, String> {
    let (kind, id) = raw
        .split_once(':')
        .ok_or_else(|| format!("`{raw}` is not `<consumer_kind>:<consumer_id>`"))?;
    let consumer_kind = match kind {
        "task_invocation" => BuildLeaseConsumerKind::TaskInvocation,
        "graph_warm" => BuildLeaseConsumerKind::GraphWarm,
        "task_dispatch" => BuildLeaseConsumerKind::TaskDispatch,
        other => {
            return Err(format!(
                "unknown consumer kind `{other}`; expected one of \
                 task_invocation, graph_warm, task_dispatch"
            ));
        }
    };
    if id.is_empty() {
        return Err(format!("`{raw}` has an empty consumer id"));
    }
    Ok(BuildLeaseKey {
        consumer_kind,
        consumer_id: id.to_owned(),
    })
}

async fn run_build_lease_action(
    repository: &BuildLeaseRepository,
    action: BuildLeaseAction,
) -> Result<String, String> {
    match action {
        BuildLeaseAction::List => {
            let rows = repository
                .list_nonterminal()
                .await
                .map_err(|e| e.to_string())?;
            Ok(render_leases("nonterminal build leases", &rows))
        }
        BuildLeaseAction::Clear { leases } => {
            let keys = leases
                .iter()
                .map(|raw| parse_lease_key(raw))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("clear: {e}"))?;
            let cleared = repository
                .clear_for_operator(&keys)
                .await
                .map_err(|e| format!("clear: {e}"))?;
            // A row that was already terminal, or that does not exist, is
            // skipped rather than failed — but it is NOT reported as cleared.
            // An operator who asked for four and is told two must go look at
            // the other two instead of believing the ledger is drained.
            let mut out = format!(
                "clear: {} of {} requested lease(s) retired\n",
                cleared.len(),
                keys.len()
            );
            out.push_str(&render_leases("retired", &cleared));
            Ok(out)
        }
    }
}

fn render_leases(heading: &str, rows: &[BuildLeaseRow]) -> String {
    if rows.is_empty() {
        return format!("{heading}: <none>");
    }
    let mut out = format!("{heading}: {}\n", rows.len());
    for row in rows {
        let _ = writeln!(
            out,
            "{}:{}  state={:?} identity={} granted_at={} updated_at={}",
            row.key.consumer_kind.as_str(),
            row.key.consumer_id,
            row.state,
            row.immutable_identity,
            row.granted_at.as_deref().unwrap_or("<never>"),
            row.updated_at,
        );
    }
    out.truncate(out.trim_end().len());
    out
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

    #[tokio::test]
    async fn launcher_authority_cli_shows_and_flips_both_directions_with_epoch_fencing() {
        let db = Database::open_in_memory().expect("test database");
        let repository = LauncherAuthorityModeRepository::new(db);
        let initial = repository.read().await.unwrap().expect("seeded authority");

        let shown = run_launcher_authority_action(&repository, LauncherAuthorityAction::Show)
            .await
            .unwrap();
        assert!(shown.contains(initial.mode.as_wire()), "{shown}");
        assert!(
            shown.contains(&format!("epoch={}", initial.epoch)),
            "{shown}"
        );

        let other = match initial.mode {
            LauncherAuthorityProtocol::LeafV1 => LauncherAuthorityModeArg::ResizeV2,
            LauncherAuthorityProtocol::ResizeV2 => LauncherAuthorityModeArg::LeafV1,
        };
        run_launcher_authority_action(
            &repository,
            LauncherAuthorityAction::Set {
                mode: other,
                expected_epoch: initial.epoch,
            },
        )
        .await
        .expect("forward flip");
        let forward = repository.read().await.unwrap().unwrap();
        assert_eq!(forward.epoch, initial.epoch + 1);

        let conflict = run_launcher_authority_action(
            &repository,
            LauncherAuthorityAction::Set {
                mode: match initial.mode {
                    LauncherAuthorityProtocol::LeafV1 => LauncherAuthorityModeArg::LeafV1,
                    LauncherAuthorityProtocol::ResizeV2 => LauncherAuthorityModeArg::ResizeV2,
                },
                expected_epoch: initial.epoch,
            },
        )
        .await
        .expect_err("stale expected epoch");
        assert!(conflict.contains("epoch conflict"), "{conflict}");

        run_launcher_authority_action(
            &repository,
            LauncherAuthorityAction::Set {
                mode: match initial.mode {
                    LauncherAuthorityProtocol::LeafV1 => LauncherAuthorityModeArg::LeafV1,
                    LauncherAuthorityProtocol::ResizeV2 => LauncherAuthorityModeArg::ResizeV2,
                },
                expected_epoch: forward.epoch,
            },
        )
        .await
        .expect("rollback flip");
        assert_eq!(
            repository.read().await.unwrap().unwrap().epoch,
            initial.epoch + 2
        );
    }

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

    /// **An operator can clear a stuck build lease without writing SQL.**
    ///
    /// `deploy/kueue/preflight.sh` refuses the authority cutover (exit 30) while
    /// any nonterminal `build_leases` row exists, and its message says stale
    /// rows "must be explicitly cleared" — a remedy that, until this subtree,
    /// existed in no CLI, MCP tool, or admin route. Production was unwedged
    /// twice on 2026-07-30 by a hand-written `UPDATE`.
    ///
    /// Driven through `run_build_lease_action`, the exact path `main` takes,
    /// against a real Postgres ledger whose row is created by the production
    /// repository. The mutation this fails on is the obvious one: a `clear`
    /// whose body does nothing leaves the row nonterminal and the final `list`
    /// non-empty.
    #[tokio::test]
    async fn an_operator_can_list_and_clear_a_stuck_build_lease() {
        use djinn_db::{BuildLeaseState, QueueBuildLeaseInput, QueueBuildLeaseResult};

        let db = Database::open_in_memory().expect("test database");
        let repository = BuildLeaseRepository::new(db);
        let key = BuildLeaseKey {
            consumer_kind: BuildLeaseConsumerKind::TaskInvocation,
            consumer_id: "019fba8a-a25e-77c3-a38f-b74943e79893".to_owned(),
        };

        // The row is minted by the production queue path, not hand-written.
        let queued = repository
            .queue(&QueueBuildLeaseInput {
                key: key.clone(),
                immutable_identity: "task:t:019fba9a-5992-7083-9beb-641f878200e1:inv".to_owned(),
                queue_deadline: None,
                launch_deadline: None,
                weight: 1,
            })
            .await
            .expect("queue a lease");
        assert!(matches!(queued, QueueBuildLeaseResult::Queued { .. }));

        // `list` shows it, and shows enough to act on.
        let rendered = run_build_lease_action(&repository, BuildLeaseAction::List)
            .await
            .expect("list");
        assert!(rendered.contains(&key.consumer_id), "{rendered}");
        assert!(rendered.contains("task_invocation"), "{rendered}");

        // A typo is refused rather than reported as a successful clear of zero.
        let error = run_build_lease_action(
            &repository,
            BuildLeaseAction::Clear {
                leases: vec!["task_invocatoin:whatever".to_owned()],
            },
        )
        .await
        .expect_err("an unknown consumer kind must be refused");
        assert!(error.contains("unknown consumer kind"), "{error}");
        assert_eq!(
            repository
                .get(&key)
                .await
                .expect("read")
                .expect("row")
                .state,
            BuildLeaseState::Queued,
            "a refused clear must not mutate the ledger"
        );

        // And the real thing.
        let rendered = run_build_lease_action(
            &repository,
            BuildLeaseAction::Clear {
                leases: vec![format!("task_invocation:{}", key.consumer_id)],
            },
        )
        .await
        .expect("clear");
        assert!(rendered.contains("1 of 1"), "{rendered}");

        let row = repository.get(&key).await.expect("read").expect("row");
        assert_eq!(
            row.state,
            BuildLeaseState::Terminal,
            "the operator command must actually retire the row — this is the \
             whole point, and the SQL UPDATE it replaces"
        );
        assert_eq!(
            row.terminal_reason.as_deref(),
            Some("operator_cleared"),
            "a human intervention must never be recorded as the reclaimer \
             having proven something"
        );

        // The cutover fence now reads clean.
        let rendered = run_build_lease_action(&repository, BuildLeaseAction::List)
            .await
            .expect("list");
        assert!(
            rendered.contains("<none>"),
            "preflight.sh counts exactly this population: {rendered}"
        );

        // Clearing again is idempotent and reports honestly: nothing was
        // retired, because there was nothing left to retire.
        let rendered = run_build_lease_action(
            &repository,
            BuildLeaseAction::Clear {
                leases: vec![format!("task_invocation:{}", key.consumer_id)],
            },
        )
        .await
        .expect("repeat clear");
        assert!(rendered.contains("0 of 1"), "{rendered}");
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
