//! Agent/launcher-side projection of the durable invocation-lease authority.
//!
//! This is the read an agent/launcher performs before it decides whether to lift
//! the reserved `cpu.max` quota for a bound invocation. It lives here — beside
//! the [`InvocationLiftDecision`] contract and above `djinn-db` — so every
//! composition that launches invocations can read it without depending on the
//! coordinator crate.
//!
//! The read itself is [`DurableInvocationLiftAuthority`], and it is injected as a
//! mandatory dependency wherever an invocation runner is built. It used to be a
//! defaulted method on `SupervisorServices` that the production pod composition
//! never overrode, which made the whole mechanism inert while the authority was
//! armed — see that type's docs for goxi launcher blocker 13.
//!
//! # What the Kueue cutover changed here (S3b)
//!
//! This module used to interpret one half of a two-authority handoff: the
//! durable row carried a four-phase ring, a v0 "emergency" mode, and two
//! per-authority acknowledgement epochs, and the projection below refused to
//! lift unless the *current phase's* required acknowledgements were at the
//! current epoch.
//!
//! The Kueue cutover deleted the v0 authority. Every one of those inputs existed
//! to coordinate the handover between two authorities, so all of them were
//! retired together — see
//! `djinn_db::repositories::invocation_lease_authority` for why the
//! acknowledgements were REMOVED rather than collapsed onto the surviving one.
//! What is left is the only question that was ever the launcher's business:
//! **is the authority armed?**

use async_trait::async_trait;
use djinn_db::{
    Database, InvocationLeaseAuthorityRepository, InvocationLeaseAuthorityRow, InvocationLeaseMode,
};

use crate::services::lease::InvocationLiftDecision;

/// The ONE seam an invocation resolves its lift decision through.
///
/// # Why this is not a method on `SupervisorServices` (goxi launcher blocker 13)
///
/// It used to be — as a **defaulted** trait method returning
/// [`InvocationLiftDecision::Unleased`]. Two impls overrode it with a real
/// durable read (`DirectServices`, `WorkerSupervisorServices`) and neither was
/// ever reached: the in-pod launcher path composes its runner in
/// `ShellLaunchContext::broker_backed`, which is handed the worker's
/// `Arc<RpcServices>` — and `RpcServices` never overrode the method, so every
/// production invocation silently took the default. Production ran a fully armed
/// authority while every leaf logged `decision=Unleased authority=Unarmed`, was
/// born at `cpu.max=[max 100000]`, and never transitioned. The control plane said
/// "armed"; the mechanism was inert.
///
/// A defaulted trait method is the wrong shape for an authority: "this impl has
/// nothing to say" and "no admission control exists" are indistinguishable at the
/// call site, and adding an impl silently opts out of the feature. So the decision
/// now travels as its own **mandatory** dependency: nothing can construct an
/// invocation runner without naming the authority it will ask, and there is no
/// fallback to fall through to.
#[async_trait]
pub trait InvocationLiftAuthority: Send + Sync + 'static {
    /// Project the durable invocation-lease authority into this invocation's
    /// lift decision. Implementations fail closed to
    /// [`InvocationLiftDecision::Unleased`].
    async fn invocation_lift_decision(&self) -> InvocationLiftDecision;
}

/// The production authority: reads the durable invocation-lease authority out of
/// the **platform** database and projects it through [`evaluate_invocation_lift`].
///
/// # The database this reads MUST be the platform database
///
/// A task-run Pod has two Postgres DSNs in its environment: `DJINN_DATABASE_URL`
/// (the platform database, where the authority row lives) and `DATABASE_URL`
/// (the project's `svc-postgres` catalog-service sidecar, which has no such
/// table). This type is constructed from a [`Database`] handle, never from an
/// environment variable, precisely so the choice is made once at the composition
/// root — the in-pod worker's `bootstrap_warm_database()`, which requires
/// `DJINN_DATABASE_URL` and hard-errors without it.
///
/// # A read failure is never silent
///
/// The previous implementations did `.map_err(|_| ())` and threw the error away,
/// which made "the authority is legitimately disarmed" and "this process cannot
/// read the authority table at all" produce byte-identical behaviour and
/// byte-identical (i.e. absent) logs. It still fails closed — that part is
/// correct — but a failed read is now an `ERROR` naming the origin and the
/// database error.
pub struct DurableInvocationLiftAuthority {
    db: Database,
    /// Which composition opened this authority (`"in-pod worker"`, `"host"`), so a
    /// logged read failure names the process that failed rather than just the row.
    origin: &'static str,
}

/// What a durable authority read actually found.
///
/// A three-state result, not a `Result<Option<_>, ()>`, because the two
/// non-row states have to be **told apart by the caller and by a test**.
/// `.map_err(|_| ())` is how blocker 13 stayed invisible for four rollouts: a
/// read that failed because the process was pointed at a database with no
/// authority table produced byte-identical behaviour, and byte-identical
/// (absent) logs, to a deployment that had simply never armed it.
#[derive(Debug)]
pub enum InvocationLeaseAuthorityRead {
    /// The durable row was read. Whether it lifts is [`evaluate_invocation_lift`]'s
    /// business, not this type's.
    Row(InvocationLeaseAuthorityRow),
    /// No row exists. Legitimately disarmed; the documented state of a deployment
    /// that has never seeded the authority.
    Absent,
    /// The read itself failed. A DEFECT — an armed authority cannot take effect
    /// in this process at all — carrying the database error for the log.
    Failed(String),
}

impl DurableInvocationLiftAuthority {
    #[must_use]
    pub fn new(db: Database, origin: &'static str) -> Self {
        Self { db, origin }
    }

    /// Read the durable authority, keeping "absent" and "failed" distinguishable.
    pub async fn read_authority(&self) -> InvocationLeaseAuthorityRead {
        match InvocationLeaseAuthorityRepository::new(self.db.clone())
            .read()
            .await
        {
            Ok(Some(row)) => InvocationLeaseAuthorityRead::Row(row),
            Ok(None) => InvocationLeaseAuthorityRead::Absent,
            Err(error) => InvocationLeaseAuthorityRead::Failed(error.to_string()),
        }
    }

    /// State the read, then project it. Fails closed on both non-row states —
    /// loudly on the one that is a defect.
    #[must_use]
    pub fn log_and_project(
        origin: &str,
        read: InvocationLeaseAuthorityRead,
    ) -> InvocationLiftDecision {
        let row = match read {
            InvocationLeaseAuthorityRead::Row(row) => Ok(Some(row)),
            InvocationLeaseAuthorityRead::Absent => {
                tracing::info!(
                    origin,
                    "build admission: no durable invocation-lease authority row; invocations \
                     stay unleased"
                );
                Ok(None)
            }
            InvocationLeaseAuthorityRead::Failed(error) => {
                tracing::error!(
                    origin,
                    %error,
                    "build admission: durable invocation-lease authority read FAILED; failing \
                     closed to Unleased. This is a DEFECT, not a disarmed authority: this \
                     process cannot read the platform database's authority row (wrong DSN — \
                     e.g. a project's DATABASE_URL catalog sidecar instead of \
                     DJINN_DATABASE_URL — missing migration, or connectivity), so an armed \
                     authority cannot take effect here and every invocation runs unleased"
                );
                Err(())
            }
        };
        evaluate_invocation_lift(row)
    }
}

#[async_trait]
impl InvocationLiftAuthority for DurableInvocationLiftAuthority {
    async fn invocation_lift_decision(&self) -> InvocationLiftDecision {
        Self::log_and_project(self.origin, self.read_authority().await)
    }
}

/// Project the durable authority row into whether a bound invocation may lift
/// the launcher quota.
///
/// It fails closed on every uncertain input:
///
/// - An unreadable (`Err`) or missing (`None`) row keeps the quota unleased.
/// - [`InvocationLeaseMode::Off`] keeps the quota unleased.
/// - [`InvocationLeaseMode::Shadow`] observes only.
/// - [`InvocationLeaseMode::Enforce`] lifts.
///
/// The caller additionally requires a matching durable fencing token before it
/// acts on a [`InvocationLiftDecision::Lift`]; this function never authorizes a
/// lift on the authority alone.
///
/// # Why there is no phase or acknowledgement check any more (Kueue cutover S3b)
///
/// This used to additionally require that the row's four-phase handoff state was
/// an overlap or invocation-primary phase, AND that the acknowledgements that
/// phase required were at the current epoch. Both were properties of a handoff
/// between two authorities. The v0 authority is deleted, so:
///
/// - There is no phase to be in. Each phase named which of the two authorities
///   was primary.
/// - There is no acknowledgement to be current. The emergency ack had exactly one
///   writer, and deleting it would have dropped every invocation to `Unleased` at
///   the next epoch bump with no compile error and no failing test. Collapsing
///   onto `invocation_ack_epoch` instead would have kept that failure mode alive
///   one column over: that ack has no runtime writer either.
///
/// Against production's live row — `v1_mode Enforce`, `cap 3` — the answer is
/// `Lift`, before and after, which is what
/// `the_live_production_row_lifts_the_per_invocation_cpu_lease` locks.
///
/// # Shadow CLAMPS — it does not speed anything up
///
/// Read this before arming `shadow` in production. Only
/// [`InvocationLiftDecision::Lift`] ever raises `cpu.max`. `Shadow` binds the
/// invocation and records telemetry (the "would throttle" arms) and then leaves
/// the leaf pinned at the broker's unleased quota —
/// `UnleasedQuota::DEFAULT_MILLICORES`, i.e. **250m** — for the whole command.
/// So a rollout that arms shadow makes every leased build slower, not faster: it
/// is an observation mode whose entire purpose is to measure what enforcement
/// *would* do. This is correct by design and asserted by
/// `shadow_epoch_binds_but_never_lifts` in `djinn-agent`'s
/// `process/tests/process_lease_admission_tests.rs` — do not "fix" it.
///
/// # `Unleased` does NOT clamp (goxi launcher blocker 11)
///
/// This used to read "`Unleased` is a literal no-op with the same effect [as
/// shadow]". Both halves of that were wrong in the same way. It was not a no-op:
/// the leaf had already been born at the 250m unleased quota before this decision
/// was ever read, so `Unleased` pinned every command there for its whole life.
/// And it was not "the same effect as shadow" in intent — shadow clamps
/// deliberately to observe, whereas `Unleased` means *no admission authority
/// exists for this invocation at all*.
///
/// Production ran the launcher armed with the authority row ABSENT
/// (`djinn-server epoch show` → `invocation lease authority: <absent>`), which is
/// this function's `Ok(None)` arm. A measured leaf reached 21,130,868 usec of CPU
/// — 84x the 250,000 usec escalation threshold — with `cpu.max` never leaving
/// `25000 100000`, making an armed launcher ~16x slower than a disabled one.
///
/// `Unleased` now selects `LeaseAuthority::Unarmed` at leaf creation (see
/// `djinn-agent`'s `process_broker::birth_authority`), so the leaf is born with no
/// quota of its own and inherits the Pod's budget. It is still contained, still
/// killable, still measurable — it just costs nothing. Asserted behaviourally on
/// a real cgroup2 hierarchy by step 7 of
/// `djinn-cgroup-launcher/tests/delegated_cpu_lease_lifecycle.rs`, and on the
/// production composition by
/// `unleased_epoch_is_born_unclamped_because_no_grant_can_ever_lift_it`, in
/// `djinn-agent`'s `process/tests/process_lease_admission_tests.rs`.
#[must_use]
pub fn evaluate_invocation_lift(
    row: Result<Option<InvocationLeaseAuthorityRow>, ()>,
) -> InvocationLiftDecision {
    let Ok(Some(row)) = row else {
        return InvocationLiftDecision::Unleased;
    };
    match row.mode {
        InvocationLeaseMode::Off => InvocationLiftDecision::Unleased,
        InvocationLeaseMode::Shadow => InvocationLiftDecision::Shadow,
        InvocationLeaseMode::Enforce => InvocationLiftDecision::Lift,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// Arm the durable authority exactly the way an operator does.
    async fn arm(db: &Database, mode: InvocationLeaseMode) {
        let authority = InvocationLeaseAuthorityRepository::new(db.clone());
        let row = authority.seed_baseline().await.expect("seed the baseline");
        authority
            .set_mode_and_cap(row.epoch, mode, Some(3))
            .await
            .expect("arm the authority");
    }

    /// **THE BEHAVIOUR-PRESERVATION PROOF FOR THE KUEUE CUTOVER (S3b).**
    ///
    /// The fixture is not a synthetic armed authority: it is the verbatim
    /// durable singleton production was running on 2026-07-30 — `phase
    /// ForwardOverlap`, `epoch 14`, `v0_mode Enforce`, `v1_mode Enforce`,
    /// `cap 3`, `emergency_ack_epoch 14`, `invocation_ack_epoch 14` — written
    /// column by column as SQL literals by
    /// [`InvocationLeaseAuthorityRepository::seed_live_production_row_for_test`],
    /// retired handoff-protocol columns and all.
    ///
    /// That row must project to [`InvocationLiftDecision::Lift`] BEFORE and
    /// AFTER the v0↔v1 handoff is retired. `Lift` is what raises `cpu.max` for a
    /// bound invocation; `Unleased` selects `LeaseAuthority::Unarmed` and removes
    /// per-invocation CPU containment altogether. Neither substitution is
    /// acceptable, and neither produces a compile error — so this is the
    /// assertion that has to carry the change.
    #[tokio::test]
    async fn the_live_production_row_lifts_the_per_invocation_cpu_lease() {
        let db = Database::open_in_memory().expect("ephemeral test database");
        let row = InvocationLeaseAuthorityRepository::new(db.clone())
            .seed_live_production_row_for_test()
            .await
            .expect("write the live production row");
        assert_eq!(row.epoch, 14, "the fixture is production's epoch");
        assert_eq!(row.mode, InvocationLeaseMode::Enforce);
        assert_eq!(row.cap, Some(3), "cap 3 is production's live reference cap");

        let authority = DurableInvocationLiftAuthority::new(db, "s3b-production-row");
        assert_eq!(
            authority.invocation_lift_decision().await,
            InvocationLiftDecision::Lift,
            "the live production row must keep arming the per-invocation cgroup \
             CPU lease; Unleased here means production loses containment"
        );
    }

    /// The production authority, over a real database in the armed state, must
    /// return `Lift`.
    #[tokio::test]
    async fn an_armed_authority_lifts_through_the_durable_reader() {
        let db = Database::open_in_memory().expect("ephemeral test database");
        arm(&db, InvocationLeaseMode::Enforce).await;
        let authority = DurableInvocationLiftAuthority::new(db, "test");
        assert_eq!(
            authority.invocation_lift_decision().await,
            InvocationLiftDecision::Lift,
        );
    }

    /// The distinction `.map_err(|_| ())` destroyed, and why blocker 13 was
    /// invisible: a read that FAILED (this process cannot see the authority
    /// table — e.g. it was handed a project's `DATABASE_URL` catalog-service
    /// sidecar instead of `DJINN_DATABASE_URL`) is not the same event as an
    /// authority that is legitimately disarmed, even though both correctly fail
    /// closed. Both project to `Unleased`; only one is a defect, and they must be
    /// separable at the seam that logs them.
    #[tokio::test]
    async fn a_failed_read_is_distinguishable_from_an_absent_row_and_both_fail_closed() {
        // Legitimately disarmed: the row is gone.
        let armed = Database::open_in_memory().expect("ephemeral test database");
        arm(&armed, InvocationLeaseMode::Enforce).await;
        InvocationLeaseAuthorityRepository::new(armed.clone())
            .delete_for_test()
            .await
            .expect("delete the singleton");
        let absent = DurableInvocationLiftAuthority::new(armed, "test");
        assert!(
            matches!(
                absent.read_authority().await,
                InvocationLeaseAuthorityRead::Absent
            ),
            "a deleted row is Absent, not Failed"
        );
        assert_eq!(
            absent.invocation_lift_decision().await,
            InvocationLiftDecision::Unleased
        );

        // A DEFECT: a valid, reachable Postgres that simply is not the platform
        // database. This is the shape of the wrong-DSN hazard — the maintenance
        // database on the same server has no authority table, exactly like a
        // task-run Pod's `svc-postgres` catalog sidecar.
        let base = djinn_db::test_database_base_url();
        let trimmed = base.trim_end_matches('/');
        let server_prefix = trimmed
            .rsplit_once('/')
            .map_or(trimmed, |(prefix, _)| prefix);
        let wrong_dsn = Database::open_with_config(djinn_db::DatabaseConnectConfig::Postgres(
            djinn_db::PostgresDatabaseConfig {
                url: format!("{server_prefix}/postgres"),
            },
        ))
        .expect("open the non-platform database");
        let broken = DurableInvocationLiftAuthority::new(wrong_dsn, "wrong-dsn");
        let read = broken.read_authority().await;
        assert!(
            matches!(read, InvocationLeaseAuthorityRead::Failed(_)),
            "a database with no authority table is a FAILED read, not a disarmed \
             authority; got {read:?}"
        );
        assert_eq!(
            broken.invocation_lift_decision().await,
            InvocationLiftDecision::Unleased,
            "a failed read still fails closed — loudly, but closed"
        );
    }

    fn row(mode: InvocationLeaseMode) -> InvocationLeaseAuthorityRow {
        InvocationLeaseAuthorityRow {
            epoch: 7,
            mode,
            cap: None,
            updated_at: "now".into(),
        }
    }

    #[test]
    fn only_an_enforcing_authority_lifts() {
        // Unreadable / missing authorities keep the quota unleased.
        assert_eq!(
            evaluate_invocation_lift(Err(())),
            InvocationLiftDecision::Unleased
        );
        assert_eq!(
            evaluate_invocation_lift(Ok(None)),
            InvocationLiftDecision::Unleased
        );
        assert_eq!(
            evaluate_invocation_lift(Ok(Some(row(InvocationLeaseMode::Off)))),
            InvocationLiftDecision::Unleased
        );
        // Shadow observes but never lifts.
        assert_eq!(
            evaluate_invocation_lift(Ok(Some(row(InvocationLeaseMode::Shadow)))),
            InvocationLiftDecision::Shadow
        );
        assert_eq!(
            evaluate_invocation_lift(Ok(Some(row(InvocationLeaseMode::Enforce)))),
            InvocationLiftDecision::Lift
        );
    }

    /// **The arming decision must survive an epoch bump.**
    ///
    /// The failure this guards is the reason the S3a/S3b split exists: under the
    /// retired handoff protocol, any epoch bump cleared the acknowledgements and
    /// silently dropped every invocation to `Unleased` — no quota of its own, no
    /// containment — until some other writer re-acknowledged. There is no such
    /// writer left, and there is no acknowledgement left either, so the bump is
    /// now inert with respect to arming. Assert that, in both directions, so a
    /// reintroduced staleness check fails here.
    #[tokio::test]
    async fn an_epoch_bump_cannot_disarm_the_invocation_lease() {
        let db = Database::open_in_memory().expect("ephemeral test database");
        let authority = InvocationLeaseAuthorityRepository::new(db.clone());
        let row = authority
            .seed_live_production_row_for_test()
            .await
            .expect("live production row");
        let reader = DurableInvocationLiftAuthority::new(db, "epoch-bump");
        assert_eq!(
            reader.invocation_lift_decision().await,
            InvocationLiftDecision::Lift,
            "precondition: production's row lifts"
        );

        // An operator changes the cap. This bumps the epoch — the exact mutation
        // that used to disarm the lease.
        let bumped = authority
            .set_mode_and_cap(row.epoch, InvocationLeaseMode::Enforce, Some(12))
            .await
            .expect("operator raises the cap");
        assert_eq!(bumped.epoch, row.epoch + 1, "the epoch really did move");
        assert_eq!(
            reader.invocation_lift_decision().await,
            InvocationLiftDecision::Lift,
            "an epoch bump must not disarm the per-invocation cgroup CPU lease"
        );

        // And the operator kill switch still works, so this is not a constant.
        authority
            .set_mode_and_cap(bumped.epoch, InvocationLeaseMode::Off, bumped.cap)
            .await
            .expect("operator disarms");
        assert_eq!(
            reader.invocation_lift_decision().await,
            InvocationLiftDecision::Unleased,
            "the kill switch must still disarm; a hard-coded Lift would remove it"
        );
    }
}
