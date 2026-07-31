//! Operator control surface for the durable invocation-lease authority.
//!
//! This is the engine behind `djinn-server epoch {show,seed,arm,set-cap,
//! kill-switch}`. It re-implements no fencing: every mutation is the same
//! epoch-fenced compare-and-swap that
//! [`InvocationLeaseAuthorityRepository::set_mode_and_cap`] already serializes
//! on one row lock, so two operators cannot interleave into a contradictory
//! committed state.
//!
//! # What it replaced
//!
//! This module used to be `build_admission_transition.rs`: a safe-ordering
//! executor for a two-authority handoff, with nine multi-step workflows
//! (`arm_shadow`, `arm_overlap`, `enter_forward_overlap`,
//! `commit_invocation_primary`, `observe_v0`, `arm_rollback`,
//! `enter_rollback_overlap`, `complete_rollback`, `abort_v1_arming`) driving a
//! four-phase ring, and error variants for waiting on the other authority's
//! acknowledgement.
//!
//! Every one of those steps existed to make the handover between two authorities
//! safe at every committed point — "at least one authority always enforces". The
//! Kueue cutover deleted the v0 authority. With one authority left the whole
//! ordering problem is gone: arming and disarming are single, epoch-fenced
//! writes, and there is no second authority to confirm, wait for, or hand back
//! to.
//!
//! What survives is what an operator actually needs during an incident: read it,
//! create it, change the cap it enforces, and turn it off.

use std::sync::Arc;

use djinn_db::error::DbError;
use djinn_db::{
    InvocationLeaseAuthorityRepository, InvocationLeaseAuthorityRow, InvocationLeaseMode,
};

use crate::build_admission::validate_reference_cap;

/// Why an operator control action could not complete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlError {
    /// The durable authority row does not exist. `seed` creates it; startup
    /// deliberately never does.
    AuthorityAbsent,
    /// A stale-epoch compare-and-swap was rejected: someone else committed a
    /// change between the operator's read and their write.
    StaleEpoch { expected: i64, current: i64 },
    /// A configuration was rejected before it could reach the durable row.
    InvalidConfig(String),
    /// An underlying storage failure that is not an epoch rejection.
    Storage(String),
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AuthorityAbsent => {
                write!(
                    f,
                    "the invocation lease authority row is absent; run `seed`"
                )
            }
            Self::StaleEpoch { expected, current } => write!(
                f,
                "stale epoch {expected}; the authority is now at epoch {current}. Re-read with \
                 `show` and retry"
            ),
            Self::InvalidConfig(msg) => write!(f, "invalid configuration: {msg}"),
            Self::Storage(msg) => write!(f, "storage error: {msg}"),
        }
    }
}

impl std::error::Error for ControlError {}

fn map_db(err: DbError) -> ControlError {
    match err {
        DbError::InvalidTransition(msg) => ControlError::Storage(msg),
        other => ControlError::Storage(other.to_string()),
    }
}

/// Composes the durable authority primitives into the operator surface.
pub struct InvocationLeaseControl {
    repo: Arc<InvocationLeaseAuthorityRepository>,
}

impl InvocationLeaseControl {
    #[must_use]
    pub fn new(repo: Arc<InvocationLeaseAuthorityRepository>) -> Self {
        Self { repo }
    }

    /// Read the durable authority for `epoch show`.
    pub async fn show(&self) -> Result<Option<InvocationLeaseAuthorityRow>, ControlError> {
        self.repo.read().await.map_err(map_db)
    }

    /// Create the durable authority row at its DISARMED baseline.
    ///
    /// Idempotent: an existing row is returned untouched, so this can never
    /// overwrite a live rollout. Startup deliberately never re-creates an absent
    /// row — removing it is the documented remediation for a wedged authority —
    /// so restoring it is an explicit operator action rather than an implicit
    /// deploy-time side effect.
    pub async fn seed(&self) -> Result<InvocationLeaseAuthorityRow, ControlError> {
        self.repo.seed_baseline().await.map_err(map_db)
    }

    /// Read the current row and fail fast if it no longer matches the epoch the
    /// operator observed, before any mutation is issued.
    async fn current(
        &self,
        expected_epoch: i64,
    ) -> Result<InvocationLeaseAuthorityRow, ControlError> {
        let row = self
            .repo
            .read()
            .await
            .map_err(map_db)?
            .ok_or(ControlError::AuthorityAbsent)?;
        if row.epoch != expected_epoch {
            return Err(ControlError::StaleEpoch {
                expected: expected_epoch,
                current: row.epoch,
            });
        }
        Ok(row)
    }

    /// Set the arming mode, optionally changing the reference cap.
    ///
    /// `cap = None` preserves the current cap. Arming to
    /// [`InvocationLeaseMode::Shadow`] or [`InvocationLeaseMode::Enforce`]
    /// requires a concrete cap — an armed authority with no cap would fall back
    /// to whatever the process happened to be configured with, which is exactly
    /// the "armed but nobody knows to what" state an operator reaches for this
    /// command to escape.
    pub async fn arm(
        &self,
        expected_epoch: i64,
        mode: InvocationLeaseMode,
        cap: Option<i64>,
    ) -> Result<InvocationLeaseAuthorityRow, ControlError> {
        let row = self.current(expected_epoch).await?;
        let cap = cap.or(row.cap);
        if mode != InvocationLeaseMode::Off {
            let Some(cap) = cap else {
                return Err(ControlError::InvalidConfig(
                    "arming needs a concrete reference cap; pass --cap N".into(),
                ));
            };
            validate_reference_cap(cap).map_err(ControlError::InvalidConfig)?;
        }
        self.repo
            .set_mode_and_cap(expected_epoch, mode, cap)
            .await
            .map_err(map_db)
    }

    /// The kill switch: disarm the authority in one epoch-fenced write.
    ///
    /// The cap is preserved rather than cleared, so re-arming does not have to
    /// re-derive a number in the middle of an incident.
    ///
    /// This used to be a three-step reverse ordering (`arm_rollback` →
    /// `enter_rollback_overlap` → `complete_rollback`) that re-confirmed the v0
    /// authority before releasing v1, because disabling v1 while v0 was merely
    /// observing would have left zero enforcing authorities. There is no v0 to
    /// re-confirm and no invariant left to violate: disarming means invocations
    /// run unleased, which is the documented state of every deployment that has
    /// never armed the authority.
    pub async fn kill_switch(
        &self,
        expected_epoch: i64,
    ) -> Result<InvocationLeaseAuthorityRow, ControlError> {
        let row = self.current(expected_epoch).await?;
        if row.mode == InvocationLeaseMode::Off {
            return Ok(row);
        }
        self.repo
            .set_mode_and_cap(expected_epoch, InvocationLeaseMode::Off, row.cap)
            .await
            .map_err(map_db)
    }

    /// Change the reference cap while preserving the arming mode.
    ///
    /// This is the operator control the 2026-07-25 incident was about. The write
    /// here is only half of it: `BuildLeaseService::refresh_epoch_cap` is what
    /// makes the new value the cap the process actually enforces, without a
    /// restart. A cap that is stored but never adopted is not a control.
    pub async fn set_cap(
        &self,
        expected_epoch: i64,
        cap: i64,
    ) -> Result<InvocationLeaseAuthorityRow, ControlError> {
        let row = self.current(expected_epoch).await?;
        validate_reference_cap(cap).map_err(ControlError::InvalidConfig)?;
        self.repo
            .set_mode_and_cap(expected_epoch, row.mode, Some(cap))
            .await
            .map_err(map_db)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_db::Database;

    async fn control() -> InvocationLeaseControl {
        let repo = Arc::new(InvocationLeaseAuthorityRepository::new(
            Database::open_in_memory().unwrap(),
        ));
        InvocationLeaseControl::new(repo)
    }

    #[tokio::test]
    async fn arming_and_the_kill_switch_are_both_reachable_from_the_baseline() {
        let control = control().await;
        let baseline = control.show().await.unwrap().unwrap();
        assert_eq!(baseline.mode, InvocationLeaseMode::Off);

        let armed = control
            .arm(baseline.epoch, InvocationLeaseMode::Enforce, Some(3))
            .await
            .unwrap();
        assert_eq!(armed.mode, InvocationLeaseMode::Enforce);
        assert_eq!(armed.cap, Some(3));

        // The kill switch preserves the cap so re-arming needs no new number.
        let disarmed = control.kill_switch(armed.epoch).await.unwrap();
        assert_eq!(disarmed.mode, InvocationLeaseMode::Off);
        assert_eq!(disarmed.cap, Some(3));

        // Re-running the kill switch converges rather than erroring, so a
        // retried step mid-incident is safe.
        let again = control.kill_switch(disarmed.epoch).await.unwrap();
        assert_eq!(again.epoch, disarmed.epoch);
        assert_eq!(again.mode, InvocationLeaseMode::Off);
    }

    #[tokio::test]
    async fn a_stale_epoch_is_rejected_before_any_mutation() {
        let control = control().await;
        let baseline = control.show().await.unwrap().unwrap();
        control
            .arm(baseline.epoch, InvocationLeaseMode::Enforce, Some(3))
            .await
            .unwrap();

        assert!(matches!(
            control.set_cap(baseline.epoch, 9).await,
            Err(ControlError::StaleEpoch { .. })
        ));
        let after = control.show().await.unwrap().unwrap();
        assert_eq!(after.cap, Some(3), "the rejected write changed nothing");
    }

    #[tokio::test]
    async fn an_out_of_range_or_missing_cap_is_refused_before_it_reaches_the_row() {
        let control = control().await;
        let baseline = control.show().await.unwrap().unwrap();
        assert!(matches!(
            control
                .arm(baseline.epoch, InvocationLeaseMode::Enforce, Some(0))
                .await,
            Err(ControlError::InvalidConfig(_))
        ));
        assert!(matches!(
            control
                .arm(
                    baseline.epoch,
                    InvocationLeaseMode::Enforce,
                    Some(1_000_000)
                )
                .await,
            Err(ControlError::InvalidConfig(_))
        ));
        // The seeded baseline has no cap, so arming without one is refused
        // rather than silently armed against the process configuration.
        assert!(matches!(
            control
                .arm(baseline.epoch, InvocationLeaseMode::Enforce, None)
                .await,
            Err(ControlError::InvalidConfig(_))
        ));
        assert_eq!(
            control.show().await.unwrap().unwrap(),
            baseline,
            "no rejected configuration reached the durable row"
        );
    }

    /// `set_cap` preserves the mode, in both directions. An armed authority must
    /// not be disarmed by a cap change, and a disarmed one must not be armed by
    /// it.
    #[tokio::test]
    async fn changing_the_cap_never_changes_the_arming_mode() {
        let control = control().await;
        let row = control.show().await.unwrap().unwrap();
        let row = control.set_cap(row.epoch, 4).await.unwrap();
        assert_eq!(
            row.mode,
            InvocationLeaseMode::Off,
            "a cap change never arms"
        );
        assert_eq!(row.cap, Some(4));

        let row = control
            .arm(row.epoch, InvocationLeaseMode::Enforce, None)
            .await
            .unwrap();
        assert_eq!(row.cap, Some(4), "arming inherits the stored cap");
        let row = control.set_cap(row.epoch, 12).await.unwrap();
        assert_eq!(
            row.mode,
            InvocationLeaseMode::Enforce,
            "a cap change never disarms"
        );
        assert_eq!(row.cap, Some(12));
    }

    #[tokio::test]
    async fn an_absent_authority_is_named_rather_than_reported_as_a_storage_error() {
        let control = control().await;
        control.repo.delete_for_test().await.unwrap();
        assert!(matches!(
            control.set_cap(0, 3).await,
            Err(ControlError::AuthorityAbsent)
        ));
        // And `seed` is the documented way back, landing disarmed.
        let seeded = control.seed().await.unwrap();
        assert_eq!(seeded.mode, InvocationLeaseMode::Off);
    }
}
