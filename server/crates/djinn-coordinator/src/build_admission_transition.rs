//! Operator-driven safe-ordering executor for the durable admission epoch.
//!
//! This module sits beside [`crate::build_admission_handoff`] (the emergency-side
//! policy interpreter) and composes the three durable primitives that
//! [`AdmissionHandoffRepository`] exposes —
//! [`AdmissionHandoffRepository::set_modes_and_cap`],
//! [`AdmissionHandoffRepository::acknowledge`], and
//! [`AdmissionHandoffRepository::advance`] — into the multi-step operator
//! workflows the rollout requires. It re-implements none of the fencing: every
//! mutation is the same epoch-fenced compare-and-swap the repository already
//! serializes on one row lock, so a cap change and a phase advance can never
//! interleave into a contradictory committed state.
//!
//! ## Authority acknowledgements
//!
//! Two acknowledgement writers exist. The v0 (emergency) authority ack is
//! written by the coordinator leader's live handoff loop
//! (`finalize_build_admission_handoff`) once the emergency controller is a
//! *healthy* `Enforce` — the executor therefore never fabricates it, it *waits*
//! for it (returning [`TransitionError::AwaitingEmergencyAck`]). This is the
//! "controller-replica ack" the reverse ordering confirms before it disables
//! v1. The v1 (invocation) authority ack has no separate runtime writer — the
//! agents read the epoch directly through
//! `evaluate_invocation_lift` — so the executor records it as it drives each
//! transition, representing the operator arming the invocation authority.
//!
//! ## Forward cutover
//!
//! `arm_shadow` → `arm_overlap` → `enter_forward_overlap` →
//! `commit_invocation_primary` → `observe_v0`. `commit_invocation_primary`
//! reaches [`AdmissionHandoffPhase::InvocationPrimary`] only after modes/cap and
//! every armed live generation have acknowledged the current epoch.
//!
//! ## Reverse kill-switch / rollback
//!
//! `arm_rollback` (commit v0 `Enforce` at a same-or-lower cap) →
//! `enter_rollback_overlap` (**halts** unless v0 is confirmed enforcing with a
//! replica ack, leaving both authorities enforcing and v1 un-disabled) →
//! `complete_rollback` (disable v1 only after the overlap confirms v0 again).
//! No transaction lifts or disables v1 quota before v0 is confirmed.

use std::sync::Arc;

use djinn_db::error::DbError;
use djinn_db::{
    AdmissionHandoffAuthority, AdmissionHandoffPhase, AdmissionHandoffRepository,
    AdmissionHandoffRow, V0Mode, V1Mode,
};

use crate::build_admission::validate_admission_config;

/// Why an operator transition could not complete.
///
/// Every variant is a bounded, actionable classification: a stale epoch or
/// illegal edge is [`Self::InvalidTransition`]; a step that is blocked waiting
/// for an external acknowledgement is [`Self::AwaitingEmergencyAck`] or
/// [`Self::AwaitingGenerationAcks`]; a rollback that refused to disable v1
/// because v0 was not confirmed is [`Self::HaltedV0Unconfirmed`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransitionError {
    /// A stale-epoch or illegal-edge compare-and-swap was rejected by the
    /// durable layer (or the observed epoch no longer matches the operator's).
    InvalidTransition(String),
    /// The v0 (emergency) controller-replica has not acknowledged the current
    /// epoch yet. Retry once the coordinator leader re-acknowledges.
    AwaitingEmergencyAck { epoch: i64 },
    /// Not every armed live generation has acknowledged the current epoch.
    AwaitingGenerationAcks { epoch: i64, missing: usize },
    /// A rollback step halted before touching v1 because v0 could not be
    /// confirmed enforcing at the current epoch. Both authorities remain
    /// enforcing and no v1 quota was lifted or disabled.
    HaltedV0Unconfirmed { epoch: i64 },
    /// A configuration was rejected before it could reach the durable row.
    InvalidConfig(String),
    /// A rollback cap must be the same as or lower than the current cap.
    CapNotSameOrLower { requested: i64, current: i64 },
    /// An underlying storage failure that is not an epoch/edge rejection.
    Storage(String),
}

impl std::fmt::Display for TransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidTransition(msg) => write!(f, "invalid transition: {msg}"),
            Self::AwaitingEmergencyAck { epoch } => {
                write!(f, "awaiting v0 emergency acknowledgement at epoch {epoch}")
            }
            Self::AwaitingGenerationAcks { epoch, missing } => write!(
                f,
                "awaiting {missing} live-generation acknowledgement(s) at epoch {epoch}"
            ),
            Self::HaltedV0Unconfirmed { epoch } => write!(
                f,
                "halted at epoch {epoch}: v0 not confirmed enforcing; both authorities remain \
                 enforcing and v1 was not disabled"
            ),
            Self::InvalidConfig(msg) => write!(f, "invalid admission configuration: {msg}"),
            Self::CapNotSameOrLower { requested, current } => write!(
                f,
                "rollback cap {requested} exceeds the current cap {current}; a rollback must not \
                 raise the cap"
            ),
            Self::Storage(msg) => write!(f, "storage error: {msg}"),
        }
    }
}

impl std::error::Error for TransitionError {}

fn map_db(err: DbError) -> TransitionError {
    match err {
        DbError::InvalidTransition(msg) => TransitionError::InvalidTransition(msg),
        other => TransitionError::Storage(other.to_string()),
    }
}

/// Validate a mode/cap combination before it can reach the durable row.
///
/// Reuses [`validate_admission_config`] when a concrete cap is present (rejecting
/// the illegal both-non-enforcing combo and an out-of-range cap up front); when
/// the cap is inherited (`None`) only the mode combo is checked.
fn validate_config(v0: V0Mode, v1: V1Mode, cap: Option<i64>) -> Result<(), TransitionError> {
    match cap {
        Some(cap) => validate_admission_config(v0, v1, cap).map_err(TransitionError::InvalidConfig),
        None if !v0.is_enforcing() && !v1.is_enforcing() => Err(TransitionError::InvalidConfig(
            format!("neither authority enforces (v0={v0:?}, v1={v1:?})"),
        )),
        None => Ok(()),
    }
}

/// Composes the durable epoch primitives into safe operator workflows.
pub struct AdmissionTransitionExecutor {
    repo: Arc<AdmissionHandoffRepository>,
}

impl AdmissionTransitionExecutor {
    #[must_use]
    pub fn new(repo: Arc<AdmissionHandoffRepository>) -> Self {
        Self { repo }
    }

    /// Read the durable row for `epoch show`.
    pub async fn show(&self) -> Result<Option<AdmissionHandoffRow>, TransitionError> {
        self.repo.read().await.map_err(map_db)
    }

    /// Read the current row and fail fast if it no longer matches the epoch the
    /// operator observed — a stale-epoch attempt returns `InvalidTransition`
    /// before any mutation is issued.
    async fn current(&self, expected_epoch: i64) -> Result<AdmissionHandoffRow, TransitionError> {
        let row = self.repo.read().await.map_err(map_db)?.ok_or_else(|| {
            TransitionError::InvalidTransition("admission handoff row absent".into())
        })?;
        if row.epoch != expected_epoch {
            return Err(TransitionError::InvalidTransition(format!(
                "stale epoch {expected_epoch}; current epoch is {}",
                row.epoch
            )));
        }
        Ok(row)
    }

    fn require_phase(
        row: &AdmissionHandoffRow,
        expected: AdmissionHandoffPhase,
    ) -> Result<(), TransitionError> {
        if row.phase == expected {
            Ok(())
        } else {
            Err(TransitionError::InvalidTransition(format!(
                "expected phase {expected:?} but the durable phase is {:?}",
                row.phase
            )))
        }
    }

    /// Record the v1 (invocation) authority ack at `epoch` and return the row as
    /// it stands after the ack, so callers can return a final state consistent
    /// with the acknowledgement they just wrote.
    async fn record_invocation_ack(
        &self,
        epoch: i64,
    ) -> Result<AdmissionHandoffRow, TransitionError> {
        self.repo
            .acknowledge(AdmissionHandoffAuthority::Invocation, epoch)
            .await
            .map_err(map_db)
    }

    // ── Forward cutover ────────────────────────────────────────────────────

    /// Arm the v1 shadow: v0 stays `Enforce`, v1 moves to `Shadow`. The phase
    /// stays `EmergencyPrimary`, so v0 remains the sole enforcing authority
    /// while v1 observes every spawn through the broker without lifting.
    pub async fn arm_shadow(
        &self,
        expected_epoch: i64,
        cap: i64,
    ) -> Result<AdmissionHandoffRow, TransitionError> {
        let row = self.current(expected_epoch).await?;
        Self::require_phase(&row, AdmissionHandoffPhase::EmergencyPrimary)?;
        validate_config(V0Mode::Enforce, V1Mode::Shadow, Some(cap))?;
        self.repo
            .set_modes_and_cap(expected_epoch, V0Mode::Enforce, V1Mode::Shadow, Some(cap))
            .await
            .map_err(map_db)
    }

    /// Arm the conservative overlap modes: both authorities `Enforce` at `cap`.
    /// The phase stays `EmergencyPrimary` (v0 still primary) until
    /// [`Self::enter_forward_overlap`] advances it, so this step alone never
    /// lets v1 lift.
    pub async fn arm_overlap(
        &self,
        expected_epoch: i64,
        cap: i64,
    ) -> Result<AdmissionHandoffRow, TransitionError> {
        let row = self.current(expected_epoch).await?;
        Self::require_phase(&row, AdmissionHandoffPhase::EmergencyPrimary)?;
        validate_config(V0Mode::Enforce, V1Mode::Enforce, Some(cap))?;
        self.repo
            .set_modes_and_cap(expected_epoch, V0Mode::Enforce, V1Mode::Enforce, Some(cap))
            .await
            .map_err(map_db)
    }

    /// Advance `EmergencyPrimary` → `ForwardOverlap` once the v0 controller-replica
    /// has acknowledged the current epoch, then record the v1 authority ack so
    /// the overlap is complete. Returns [`TransitionError::AwaitingEmergencyAck`]
    /// until the leader acknowledges.
    pub async fn enter_forward_overlap(
        &self,
        expected_epoch: i64,
    ) -> Result<AdmissionHandoffRow, TransitionError> {
        let row = self.current(expected_epoch).await?;
        Self::require_phase(&row, AdmissionHandoffPhase::EmergencyPrimary)?;
        if row.emergency_ack_epoch != Some(row.epoch) {
            return Err(TransitionError::AwaitingEmergencyAck { epoch: row.epoch });
        }
        let overlap = self
            .repo
            .advance(expected_epoch, AdmissionHandoffPhase::ForwardOverlap, &[])
            .await
            .map_err(map_db)?;
        self.record_invocation_ack(overlap.epoch).await
    }

    /// Commit `ForwardOverlap` → `InvocationPrimary`. This edge opens only after
    /// modes/cap plus both authority acks are current AND every generation in
    /// `armed_generations` — the live set captured when the overlap was armed —
    /// has an idempotent current-epoch acknowledgement. It records the v1
    /// authority ack on the committed invocation-primary epoch so the invocation
    /// authority keeps lifting.
    pub async fn commit_invocation_primary(
        &self,
        expected_epoch: i64,
        armed_generations: &[String],
    ) -> Result<AdmissionHandoffRow, TransitionError> {
        let row = self.current(expected_epoch).await?;
        Self::require_phase(&row, AdmissionHandoffPhase::ForwardOverlap)?;
        if row.emergency_ack_epoch != Some(row.epoch) {
            return Err(TransitionError::AwaitingEmergencyAck { epoch: row.epoch });
        }
        // The v1 authority ack is the executor's to write; re-assert it so a
        // retry after a cleared ack still makes progress.
        if row.invocation_ack_epoch != Some(row.epoch) {
            self.record_invocation_ack(row.epoch).await?;
        }
        if !self
            .repo
            .generation_ack_complete(row.epoch, armed_generations)
            .await
            .map_err(map_db)?
        {
            // Report how many are still missing without leaking generation
            // identities into the error.
            let missing = self
                .count_missing_generation_acks(row.epoch, armed_generations)
                .await?;
            return Err(TransitionError::AwaitingGenerationAcks {
                epoch: row.epoch,
                missing,
            });
        }
        let primary = self
            .repo
            .advance(
                expected_epoch,
                AdmissionHandoffPhase::InvocationPrimary,
                armed_generations,
            )
            .await
            .map_err(map_db)?;
        self.record_invocation_ack(primary.epoch).await
    }

    async fn count_missing_generation_acks(
        &self,
        epoch: i64,
        armed_generations: &[String],
    ) -> Result<usize, TransitionError> {
        let mut missing = 0;
        for key in armed_generations {
            if !self
                .repo
                .generation_ack_complete(epoch, std::slice::from_ref(key))
                .await
                .map_err(map_db)?
            {
                missing += 1;
            }
        }
        Ok(missing)
    }

    /// Move v0 to `Observe` after invocation-primary is committed: v1 keeps
    /// `Enforce`, v0 stops denying. This is the terminal forward step and
    /// preserves the current reference cap. Re-records the v1 authority ack on
    /// the new epoch so the invocation authority keeps lifting.
    pub async fn observe_v0(
        &self,
        expected_epoch: i64,
    ) -> Result<AdmissionHandoffRow, TransitionError> {
        let row = self.current(expected_epoch).await?;
        Self::require_phase(&row, AdmissionHandoffPhase::InvocationPrimary)?;
        validate_config(V0Mode::Observe, V1Mode::Enforce, row.cap)?;
        let updated = self
            .repo
            .set_modes_and_cap(expected_epoch, V0Mode::Observe, V1Mode::Enforce, row.cap)
            .await
            .map_err(map_db)?;
        self.record_invocation_ack(updated.epoch).await
    }

    // ── Reverse kill-switch / rollback ─────────────────────────────────────

    /// Begin a rollback from `InvocationPrimary`: commit v0 back to `Enforce`
    /// (both authorities enforcing) at a **same-or-lower** cap. This is the
    /// first, safe half of the reverse ordering: v0 re-enforces and v1 keeps
    /// enforcing, so at least one — here both — authority enforces at every
    /// committed point. It never disables v1.
    pub async fn arm_rollback(
        &self,
        expected_epoch: i64,
        cap: i64,
    ) -> Result<AdmissionHandoffRow, TransitionError> {
        let row = self.current(expected_epoch).await?;
        Self::require_phase(&row, AdmissionHandoffPhase::InvocationPrimary)?;
        if let Some(current) = row.cap
            && cap > current
        {
            return Err(TransitionError::CapNotSameOrLower {
                requested: cap,
                current,
            });
        }
        validate_config(V0Mode::Enforce, V1Mode::Enforce, Some(cap))?;
        self.repo
            .set_modes_and_cap(expected_epoch, V0Mode::Enforce, V1Mode::Enforce, Some(cap))
            .await
            .map_err(map_db)
    }

    /// Advance `InvocationPrimary` → `RollbackOverlap` **only** after confirming
    /// v0 is enforcing with a controller-replica acknowledgement at the current
    /// epoch. If v0 is not confirmed this returns
    /// [`TransitionError::HaltedV0Unconfirmed`] and makes no mutation — both
    /// authorities remain enforcing and v1 is not disabled.
    pub async fn enter_rollback_overlap(
        &self,
        expected_epoch: i64,
    ) -> Result<AdmissionHandoffRow, TransitionError> {
        let row = self.current(expected_epoch).await?;
        Self::require_phase(&row, AdmissionHandoffPhase::InvocationPrimary)?;
        let v0_confirmed =
            row.v0_mode == V0Mode::Enforce && row.emergency_ack_epoch == Some(row.epoch);
        if !v0_confirmed {
            return Err(TransitionError::HaltedV0Unconfirmed { epoch: row.epoch });
        }
        // Leaving InvocationPrimary requires the v1 authority ack at the current
        // epoch; record it now that v0 is confirmed. The v0 controller stays
        // durably `Enforce` and the epoch is unchanged, so the advance commits
        // with both authorities enforcing even if the leader's in-memory
        // controller briefly observes the now-complete InvocationPrimary.
        self.record_invocation_ack(row.epoch).await?;
        let overlap = self
            .repo
            .advance(expected_epoch, AdmissionHandoffPhase::RollbackOverlap, &[])
            .await
            .map_err(map_db)?;
        self.record_invocation_ack(overlap.epoch).await
    }

    /// Complete the rollback from `RollbackOverlap`: confirm v0 once more (both
    /// authorities acked), advance `RollbackOverlap` → `EmergencyPrimary` (at
    /// which point v1 stops lifting regardless of its mode), then set v1 to
    /// `Off` to formally disable it. Returns
    /// [`TransitionError::AwaitingEmergencyAck`] until v0 is re-confirmed, never
    /// disabling v1 without it.
    pub async fn complete_rollback(
        &self,
        expected_epoch: i64,
    ) -> Result<AdmissionHandoffRow, TransitionError> {
        let row = self.current(expected_epoch).await?;
        Self::require_phase(&row, AdmissionHandoffPhase::RollbackOverlap)?;
        if row.v0_mode != V0Mode::Enforce || row.emergency_ack_epoch != Some(row.epoch) {
            return Err(TransitionError::AwaitingEmergencyAck { epoch: row.epoch });
        }
        // Leaving the RollbackOverlap requires BOTH authority acks; v0 is
        // confirmed above, so record the v1 authority ack before the advance.
        self.record_invocation_ack(row.epoch).await?;
        let baseline = self
            .repo
            .advance(expected_epoch, AdmissionHandoffPhase::EmergencyPrimary, &[])
            .await
            .map_err(map_db)?;
        // v1 already stopped lifting the moment the phase became EmergencyPrimary;
        // formally disable it so the durable modes match the v0-only baseline.
        validate_config(V0Mode::Enforce, V1Mode::Off, baseline.cap)?;
        self.repo
            .set_modes_and_cap(baseline.epoch, V0Mode::Enforce, V1Mode::Off, baseline.cap)
            .await
            .map_err(map_db)
    }

    // ── Cap changes (serialize with the handoff edge) ──────────────────────

    /// Change the reference cap while preserving the current modes. Because this
    /// is the same epoch-fenced compare-and-swap as a phase advance, a cap change
    /// and an advance can never interleave into a contradictory committed state:
    /// whichever commits first bumps the epoch and the other is rejected as a
    /// stale-epoch [`TransitionError::InvalidTransition`]. Re-records the v1
    /// authority ack when v1 was authoritative so the invocation side keeps
    /// lifting across the cap change.
    pub async fn set_cap(
        &self,
        expected_epoch: i64,
        cap: i64,
    ) -> Result<AdmissionHandoffRow, TransitionError> {
        let row = self.current(expected_epoch).await?;
        validate_config(row.v0_mode, row.v1_mode, Some(cap))?;
        let updated = self
            .repo
            .set_modes_and_cap(expected_epoch, row.v0_mode, row.v1_mode, Some(cap))
            .await
            .map_err(map_db)?;
        // Preserve v1 authority continuity across a cap change in the phases
        // where the invocation side must stay acknowledged to keep lifting.
        let needs_invocation_ack = matches!(
            updated.phase,
            AdmissionHandoffPhase::ForwardOverlap
                | AdmissionHandoffPhase::InvocationPrimary
                | AdmissionHandoffPhase::RollbackOverlap
        );
        if needs_invocation_ack {
            return self.record_invocation_ack(updated.epoch).await;
        }
        Ok(updated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_db::Database;

    async fn executor() -> AdmissionTransitionExecutor {
        let repo = Arc::new(AdmissionHandoffRepository::new(
            Database::open_in_memory().unwrap(),
        ));
        AdmissionTransitionExecutor::new(repo)
    }

    /// Simulate the coordinator leader's live handoff loop acknowledging the v0
    /// (emergency) authority at the row's current epoch.
    async fn leader_acks_emergency(exec: &AdmissionTransitionExecutor) -> i64 {
        let row = exec.show().await.unwrap().unwrap();
        exec.repo
            .acknowledge(AdmissionHandoffAuthority::Emergency, row.epoch)
            .await
            .unwrap();
        row.epoch
    }

    /// Drive the whole forward cutover, returning the invocation-primary row.
    async fn cutover(
        exec: &AdmissionTransitionExecutor,
        cap: i64,
        armed: &[String],
    ) -> AdmissionHandoffRow {
        // Baseline EmergencyPrimary(0) → arm shadow.
        let shadow = exec.arm_shadow(0, cap).await.unwrap();
        assert_eq!(shadow.v1_mode, V1Mode::Shadow);
        assert_eq!(shadow.phase, AdmissionHandoffPhase::EmergencyPrimary);
        // Arm overlap modes (both enforce), phase still EmergencyPrimary.
        let overlap_modes = exec.arm_overlap(shadow.epoch, cap).await.unwrap();
        assert_eq!(overlap_modes.v1_mode, V1Mode::Enforce);
        assert_eq!(overlap_modes.phase, AdmissionHandoffPhase::EmergencyPrimary);
        // Advance into ForwardOverlap requires the leader's emergency ack.
        assert!(matches!(
            exec.enter_forward_overlap(overlap_modes.epoch).await,
            Err(TransitionError::AwaitingEmergencyAck { .. })
        ));
        leader_acks_emergency(exec).await;
        let overlap = exec
            .enter_forward_overlap(overlap_modes.epoch)
            .await
            .unwrap();
        assert_eq!(overlap.phase, AdmissionHandoffPhase::ForwardOverlap);
        // Commit needs the leader ack + all generation acks.
        assert!(matches!(
            exec.commit_invocation_primary(overlap.epoch, armed).await,
            Err(TransitionError::AwaitingEmergencyAck { .. })
        ));
        leader_acks_emergency(exec).await;
        if !armed.is_empty() {
            assert!(matches!(
                exec.commit_invocation_primary(overlap.epoch, armed).await,
                Err(TransitionError::AwaitingGenerationAcks { .. })
            ));
            for key in armed {
                exec.repo
                    .record_generation_ack(overlap.epoch, key)
                    .await
                    .unwrap();
            }
        }
        let primary = exec
            .commit_invocation_primary(overlap.epoch, armed)
            .await
            .unwrap();
        assert_eq!(primary.phase, AdmissionHandoffPhase::InvocationPrimary);
        primary
    }

    /// AC1: the forward executor reaches invocation_primary only after modes/cap
    /// and all live-generation acks are committed.
    #[tokio::test]
    async fn forward_reaches_invocation_primary_only_after_all_generation_acks() {
        let exec = executor().await;
        let armed = vec![
            "warm_build/alpha/0".to_string(),
            "task_run/beta/3".to_string(),
        ];
        let primary = cutover(&exec, 3, &armed).await;
        assert_eq!(primary.v0_mode, V0Mode::Enforce);
        assert_eq!(primary.v1_mode, V1Mode::Enforce);
        assert_eq!(primary.invocation_ack_epoch, Some(primary.epoch));

        // Terminal forward step: v0 → observe, v1 stays enforcing.
        let observed = exec.observe_v0(primary.epoch).await.unwrap();
        assert_eq!(observed.v0_mode, V0Mode::Observe);
        assert_eq!(observed.v1_mode, V1Mode::Enforce);
        assert_eq!(observed.phase, AdmissionHandoffPhase::InvocationPrimary);
        assert_eq!(observed.invocation_ack_epoch, Some(observed.epoch));
    }

    /// AC1: the forward executor refuses to skip an edge, and stale-epoch /
    /// illegal-edge attempts return InvalidTransition.
    #[tokio::test]
    async fn forward_refuses_to_skip_edges_and_rejects_stale_epochs() {
        let exec = executor().await;
        // Cannot commit invocation-primary straight from the baseline phase.
        assert!(matches!(
            exec.commit_invocation_primary(0, &[]).await,
            Err(TransitionError::InvalidTransition(_))
        ));
        // Cannot enter the forward overlap from the baseline phase either
        // (no overlap modes / no emergency ack yet): the ack gate fires first.
        assert!(matches!(
            exec.enter_forward_overlap(0).await,
            Err(TransitionError::AwaitingEmergencyAck { .. })
        ));

        let shadow = exec.arm_shadow(0, 3).await.unwrap();
        // A stale epoch (the operator's observed epoch is behind the durable row)
        // is rejected before any mutation.
        assert!(matches!(
            exec.arm_overlap(0, 3).await,
            Err(TransitionError::InvalidTransition(_))
        ));
        // The correct epoch works.
        exec.arm_overlap(shadow.epoch, 3).await.unwrap();
    }

    /// AC1 config guard: the illegal both-non-enforcing combo and an
    /// out-of-range cap are rejected up front.
    #[tokio::test]
    async fn forward_rejects_illegal_config_up_front() {
        let exec = executor().await;
        // Cap out of range.
        assert!(matches!(
            exec.arm_shadow(0, 0).await,
            Err(TransitionError::InvalidConfig(_))
        ));
        assert!(matches!(
            exec.arm_shadow(0, 1_000_000).await,
            Err(TransitionError::InvalidConfig(_))
        ));
    }

    /// AC2: the rollback executor confirms v0 Enforce with replica acks before
    /// any transaction lifts/disables v1 quota; if v0 confirmation fails it
    /// halts with both authorities enforcing and never disables v1.
    #[tokio::test]
    async fn rollback_halts_with_both_enforcing_when_v0_unconfirmed() {
        let exec = executor().await;
        let primary = cutover(&exec, 3, &[]).await;
        // Post-cutover: move v0 to observe (v1 primary).
        let observed = exec.observe_v0(primary.epoch).await.unwrap();
        assert_eq!(observed.v0_mode, V0Mode::Observe);

        // Begin rollback at a same-or-lower cap: v0 back to Enforce, both enforce.
        let armed = exec.arm_rollback(observed.epoch, 2).await.unwrap();
        assert_eq!(armed.v0_mode, V0Mode::Enforce);
        assert_eq!(armed.v1_mode, V1Mode::Enforce);
        assert_eq!(armed.cap, Some(2));

        // v0 not yet confirmed (leader has not acked): the rollback HALTS and
        // makes no mutation — both authorities remain enforcing.
        assert!(matches!(
            exec.enter_rollback_overlap(armed.epoch).await,
            Err(TransitionError::HaltedV0Unconfirmed { .. })
        ));
        let after_halt = exec.show().await.unwrap().unwrap();
        assert_eq!(after_halt.epoch, armed.epoch, "halt made no mutation");
        assert_eq!(after_halt.v0_mode, V0Mode::Enforce);
        assert_eq!(after_halt.v1_mode, V1Mode::Enforce);

        // Once v0 is confirmed (leader acks), the rollback proceeds.
        leader_acks_emergency(&exec).await;
        let overlap = exec.enter_rollback_overlap(armed.epoch).await.unwrap();
        assert_eq!(overlap.phase, AdmissionHandoffPhase::RollbackOverlap);

        // Completing the rollback needs v0 re-confirmed at the overlap epoch.
        assert!(matches!(
            exec.complete_rollback(overlap.epoch).await,
            Err(TransitionError::AwaitingEmergencyAck { .. })
        ));
        leader_acks_emergency(&exec).await;
        let baseline = exec.complete_rollback(overlap.epoch).await.unwrap();
        assert_eq!(baseline.phase, AdmissionHandoffPhase::EmergencyPrimary);
        assert_eq!(baseline.v0_mode, V0Mode::Enforce);
        assert_eq!(
            baseline.v1_mode,
            V1Mode::Off,
            "v1 disabled only after v0 confirmed"
        );
    }

    /// AC2 cap guard: a rollback must not raise the cap.
    #[tokio::test]
    async fn rollback_rejects_a_higher_cap() {
        let exec = executor().await;
        let primary = cutover(&exec, 3, &[]).await;
        let observed = exec.observe_v0(primary.epoch).await.unwrap();
        assert!(matches!(
            exec.arm_rollback(observed.epoch, 4).await,
            Err(TransitionError::CapNotSameOrLower {
                requested: 4,
                current: 3
            })
        ));
    }

    /// AC3: a cap change and a phase advance cannot interleave into a
    /// contradictory committed state — the second CAS against the now-stale
    /// epoch is rejected as an InvalidTransition.
    #[tokio::test]
    async fn cap_change_and_phase_advance_do_not_interleave() {
        let exec = executor().await;
        // Reach a ForwardOverlap so both a cap change and an advance are legal.
        let shadow = exec.arm_shadow(0, 3).await.unwrap();
        let overlap_modes = exec.arm_overlap(shadow.epoch, 3).await.unwrap();
        leader_acks_emergency(&exec).await;
        let overlap = exec
            .enter_forward_overlap(overlap_modes.epoch)
            .await
            .unwrap();
        leader_acks_emergency(&exec).await;
        let observed_epoch = exec.show().await.unwrap().unwrap().epoch;
        assert_eq!(observed_epoch, overlap.epoch);

        // A cap change commits first and bumps the epoch.
        let recapped = exec.set_cap(observed_epoch, 2).await.unwrap();
        assert_eq!(recapped.cap, Some(2));
        assert_eq!(recapped.epoch, observed_epoch + 1);

        // An advance from the now-stale observed epoch is rejected: the two
        // mutations serialized and did not interleave.
        assert!(matches!(
            exec.commit_invocation_primary(observed_epoch, &[]).await,
            Err(TransitionError::InvalidTransition(_))
        ));

        // And the reverse ordering: an advance first makes a stale cap change fail.
        let exec2 = executor().await;
        let shadow2 = exec2.arm_shadow(0, 3).await.unwrap();
        let stale = shadow2.epoch;
        // A phase-adjacent mutation (arm_overlap) commits at `stale`.
        exec2.arm_overlap(stale, 3).await.unwrap();
        // A cap change against the pre-advance epoch is now stale.
        assert!(matches!(
            exec2.set_cap(stale, 2).await,
            Err(TransitionError::InvalidTransition(_))
        ));
    }

    /// A kill-switch initiated directly from invocation-primary (v0 already
    /// enforcing) confirms v0 and disables v1 through the same safe ordering.
    #[tokio::test]
    async fn kill_switch_from_invocation_primary_disables_v1_after_confirming_v0() {
        let exec = executor().await;
        let primary = cutover(&exec, 3, &[]).await;
        // Do NOT observe_v0: v0 is still Enforce from the overlap. Kill-switch
        // re-commits both-enforce at a same cap, then rolls back.
        let armed = exec.arm_rollback(primary.epoch, 3).await.unwrap();
        leader_acks_emergency(&exec).await;
        let overlap = exec.enter_rollback_overlap(armed.epoch).await.unwrap();
        leader_acks_emergency(&exec).await;
        let baseline = exec.complete_rollback(overlap.epoch).await.unwrap();
        assert_eq!(baseline.phase, AdmissionHandoffPhase::EmergencyPrimary);
        assert_eq!(baseline.v1_mode, V1Mode::Off);
        assert_eq!(baseline.v0_mode, V0Mode::Enforce);
    }
}
