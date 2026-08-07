//! The producer of the `provider_actions_drained_at` proof (proposal `nafu`,
//! wave 3).
//!
//! # Why this lives next to the executor rather than in `actor.rs`
//!
//! `provider_actions_drained_at` is not a coordinator-lifecycle fact that CI
//! routing happens to read. It is a CI-routing fact that the coordinator
//! happens to write: the column exists only so that
//! [`super::executor::CiIncarnationLiveness`] can answer
//! `CiQuiescenceProof::GracefulDrain`, and `recover_calling_owner` accepts
//! nothing else. Since the AC5 fix, the *entire* safety of `calling`-row
//! recovery rests on this one stamp, so the code that earns it and the code
//! that reads it are one contract and belong under one module — and under the
//! same acceptance filter, so a mutation to either is caught by the same
//! command.
//!
//! # The ordering, and why every step of it is load-bearing
//!
//! ```text
//! close admission -> join in-flight -> stamp drained -> release the lock
//! ```
//!
//! A stamp written before the join is a *false* proof: the next incarnation
//! reads it, takes a charged `calling` row whose `rerun_failed_jobs` future is
//! still live in the old process, discards the old owner's fenced
//! `finalize_calling`, and either runs the same provider mutation twice or
//! spends a Lead session adjudicating `outcome_unknown` an episode that
//! succeeded. That is the hazard the whole AC5 witness exists to prevent, and
//! it is reachable from here and nowhere else, because nothing else writes the
//! column.
//!
//! Which is why the failure mode here is *no stamp*, never a hedged one. An
//! unjoined drain returns [`CiDrainOutcome::NotJoined`] with the ledger
//! untouched; a later incarnation then finds `provider_actions_drained_at`
//! NULL, reads [`djinn_db::CiQuiescenceProof::None`], and defers indefinitely
//! rather than racing. That costs recovery latency — the row waits for this
//! incarnation's own fenced finalizer — and never costs exclusion.

use std::time::Duration;

use crate::types::ProviderActionScope;

/// How long shutdown waits for in-flight CI-route provider actions to finish.
///
/// Sized above the provider client's own request timeout so a call that is
/// simply slow is joined rather than abandoned, and well below any orchestrator
/// termination grace period so the bounded wait never becomes the reason a pod
/// is SIGKILLed. Exceeding it is reported, not retried: the handoff loses its
/// graceful proof and startup recovery defers.
///
/// Strictly *shorter* than leadership's own `PROVIDER_ACTION_DRAIN_WAIT`, so
/// the ordinary outcome is "the coordinator finished and stamped", never
/// "leadership gave up while the coordinator was still joining".
pub(crate) const PROVIDER_ACTION_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// What one quiesce attempt actually achieved.
///
/// Returned rather than only logged because "stamped" and "gave up" are
/// indistinguishable from the outside otherwise, and they are the two states
/// the next incarnation's recovery decision turns on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CiDrainOutcome {
    /// Admission closed, every in-flight provider action joined, and this
    /// incarnation's `provider_actions_drained_at` written. The only outcome
    /// that authorises a later `calling` handoff.
    Stamped,
    /// The join did not complete within the budget. **Nothing was stamped**,
    /// deliberately: a proof written here would name futures that are still
    /// running.
    NotJoined,
    /// The join completed but the ledger write did not land — a database
    /// failure, or an incarnation the ledger has no row for. Also unstamped,
    /// and the scope is not marked drained, because there is no proof to point
    /// a future incarnation at.
    NotStamped,
}

/// Close CI-route action admission, join every in-flight provider action, and
/// stamp this incarnation's drain proof.
///
/// # Why this runs before the coordinator loop breaks
///
/// Cancellation fires on every task watching the token at once — leadership's
/// lock-release arm included. If the drain ran after the loop exited, or off in
/// a spawned task, leadership could release the lock first and a new
/// incarnation could acquire it while a provider future was still live. The
/// coordinator's `select!` arm awaits this inline for exactly that reason, and
/// leadership waits on the scope rather than on a timer.
///
/// # Why nothing here can panic
///
/// Every step is best-effort. A failure loses the graceful proof and costs
/// recovery latency; a panic on the shutdown path would leave the process
/// wedged with the lock still held.
pub(crate) async fn quiesce_provider_actions(
    scope: &ProviderActionScope,
    incarnations: &djinn_db::CoordinatorIncarnationRepository,
    incarnation_id: &str,
    drain_timeout: Duration,
) -> CiDrainOutcome {
    // 1. No new route may enter `calling`.
    scope.close_admission();

    // 2. Join what is already inside. Bounded: a provider call runs under the
    //    client's own request timeout, so an unbounded wait here would only ever
    //    be a bug elsewhere — and hanging shutdown is a worse failure than an
    //    unstamped handoff.
    let joined = tokio::time::timeout(drain_timeout, scope.wait_until_empty())
        .await
        .is_ok();
    if !joined {
        tracing::error!(
            incarnation_id = %incarnation_id,
            in_flight = scope.in_flight(),
            "CoordinatorActor: provider-action scope did not drain; releasing leadership \
             WITHOUT a graceful-drain proof — startup recovery will defer on these rows",
        );
        return CiDrainOutcome::NotJoined;
    }

    // 3. Stamp. `mark_provider_actions_drained` refuses a row that never
    //    entered `draining`, so the two writes are ordered rather than merged.
    if let Err(error) = incarnations.mark_draining(incarnation_id).await {
        tracing::error!(incarnation_id = %incarnation_id, %error, "CoordinatorActor: failed to mark incarnation draining");
        return CiDrainOutcome::NotStamped;
    }
    let stamped = match incarnations
        .mark_provider_actions_drained(incarnation_id)
        .await
    {
        Ok(true) => true,
        // The write is write-once and fenced, so `false` means either this
        // incarnation had already stamped (a second quiesce) or the ledger
        // holds no row for it at all. Only the first is a proof, and the
        // difference is not guessable — so read the column back rather than
        // let `mark_drained` claim a stamp that a recovering incarnation would
        // never find.
        Ok(false) => matches!(
            incarnations.get(incarnation_id).await,
            Ok(Some(row)) if row.provider_actions_drained_at.is_some()
        ),
        Err(error) => {
            tracing::error!(incarnation_id = %incarnation_id, %error, "CoordinatorActor: failed to stamp provider_actions_drained_at");
            false
        }
    };
    if !stamped {
        tracing::error!(
            incarnation_id = %incarnation_id,
            "CoordinatorActor: provider actions joined but no drain stamp landed; leadership \
             releases the lock WITHOUT a graceful-drain proof",
        );
        return CiDrainOutcome::NotStamped;
    }

    // 4. Only now may leadership release the lock.
    scope.mark_drained();
    tracing::info!(
        incarnation_id = %incarnation_id,
        "CoordinatorActor: provider actions quiesced; leadership may release the lock",
    );
    CiDrainOutcome::Stamped
}
