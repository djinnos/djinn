//! Agent/launcher-side projection of the durable admission epoch.
//!
//! The v0 (emergency) side of the handoff is interpreted by the coordinator's
//! `evaluate_handoff`. This module is the v1 (invocation) counterpart read by
//! the agent/launcher before it decides whether to lift the reserved cpu.max
//! quota. It lives here — beside the [`InvocationLiftDecision`] contract and
//! above `djinn-db` — so every composition that launches invocations can read it
//! without depending on the coordinator crate.
//!
//! The read itself is [`DurableInvocationLiftAuthority`], and it is injected as a
//! mandatory dependency wherever an invocation runner is built. It used to be a
//! defaulted method on `SupervisorServices` that the production pod composition
//! never overrode, which made the whole v1 mechanism inert while the epoch was
//! armed — see that type's docs for goxi launcher blocker 13.

use async_trait::async_trait;
use djinn_db::{
    AdmissionHandoffPhase, AdmissionHandoffRepository, AdmissionHandoffRow, Database, V1Mode,
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
/// epoch (`ForwardOverlap` · epoch 3 · v1 `Enforce` · both acks at 3) while every
/// leaf logged `decision=Unleased authority=Unarmed`, was born at
/// `cpu.max=[max 100000]`, and never transitioned. The control plane said
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
    /// Project the durable admission epoch into this invocation's lift decision.
    /// Implementations fail closed to [`InvocationLiftDecision::Unleased`].
    async fn invocation_lift_decision(&self) -> InvocationLiftDecision;
}

/// The production authority: reads the durable admission epoch out of the
/// **platform** database and projects it through [`evaluate_invocation_lift`].
///
/// # The database this reads MUST be the platform database
///
/// A task-run Pod has two Postgres DSNs in its environment: `DJINN_DATABASE_URL`
/// (the platform database, where `admission_handoff` lives) and `DATABASE_URL`
/// (the project's `svc-postgres` catalog-service sidecar, which has no such
/// table). This type is constructed from a [`Database`] handle, never from an
/// environment variable, precisely so the choice is made once at the composition
/// root — the in-pod worker's `bootstrap_warm_database()`, which requires
/// `DJINN_DATABASE_URL` and hard-errors without it.
///
/// # A read failure is never silent
///
/// The previous implementations did `.map_err(|_| ())` and threw the error away,
/// which made "the epoch is legitimately unarmed" and "this process cannot read
/// the epoch table at all" produce byte-identical behaviour and byte-identical
/// (i.e. absent) logs. It still fails closed — that part is correct — but a
/// failed read is now an `ERROR` naming the origin and the database error.
pub struct DurableInvocationLiftAuthority {
    db: Database,
    /// Which composition opened this authority (`"in-pod worker"`, `"host"`), so a
    /// logged read failure names the process that failed rather than just the row.
    origin: &'static str,
}

/// What a durable epoch read actually found.
///
/// A three-state result, not a `Result<Option<_>, ()>`, because the two
/// non-row states have to be **told apart by the caller and by a test**.
/// `.map_err(|_| ())` is how blocker 13 stayed invisible for four rollouts: a
/// read that failed because the process was pointed at a database with no
/// `admission_handoff` table produced byte-identical behaviour, and byte-identical
/// (absent) logs, to a deployment that had simply never armed the epoch.
#[derive(Debug)]
pub enum AdmissionEpochRead {
    /// The durable row was read. Whether it lifts is [`evaluate_invocation_lift`]'s
    /// business, not this type's.
    Row(AdmissionHandoffRow),
    /// No row exists. Legitimately unarmed; the documented state of a deployment
    /// that has never seeded the epoch.
    Absent,
    /// The read itself failed. A DEFECT — an armed epoch cannot take effect in
    /// this process at all — carrying the database error for the log.
    Failed(String),
}

impl DurableInvocationLiftAuthority {
    #[must_use]
    pub fn new(db: Database, origin: &'static str) -> Self {
        Self { db, origin }
    }

    /// Read the durable epoch, keeping "absent" and "failed" distinguishable.
    pub async fn read_epoch(&self) -> AdmissionEpochRead {
        match AdmissionHandoffRepository::new(self.db.clone())
            .read()
            .await
        {
            Ok(Some(row)) => AdmissionEpochRead::Row(row),
            Ok(None) => AdmissionEpochRead::Absent,
            Err(error) => AdmissionEpochRead::Failed(error.to_string()),
        }
    }

    /// State the read, then project it. Fails closed on both non-row states —
    /// loudly on the one that is a defect.
    #[must_use]
    pub fn log_and_project(origin: &str, read: AdmissionEpochRead) -> InvocationLiftDecision {
        let row = match read {
            AdmissionEpochRead::Row(row) => Ok(Some(row)),
            AdmissionEpochRead::Absent => {
                tracing::info!(
                    origin,
                    "build admission: no durable admission_handoff row; invocations stay unleased"
                );
                Ok(None)
            }
            AdmissionEpochRead::Failed(error) => {
                tracing::error!(
                    origin,
                    %error,
                    "build admission: durable admission_handoff read FAILED; failing closed to \
                     Unleased. This is a DEFECT, not an unarmed epoch: this process cannot read \
                     the platform database's admission_handoff table (wrong DSN — e.g. a \
                     project's DATABASE_URL catalog sidecar instead of DJINN_DATABASE_URL — \
                     missing migration, or connectivity), so an armed epoch cannot take effect \
                     here and every invocation runs unleased"
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
        Self::log_and_project(self.origin, self.read_epoch().await)
    }
}

/// Project the durable epoch row into whether a bound v1 invocation may lift the
/// launcher quota.
///
/// It is intentionally independent of the v0 emergency gates and fails closed on
/// every uncertain input:
///
/// - An unreadable (`Err`) or missing (`None`) row keeps the quota unleased.
/// - The illegal both-non-enforcing combo keeps the quota unleased.
/// - An incomplete epoch (the current phase's required acknowledgements are not
///   at the current epoch) keeps the quota unleased — a stale epoch never lifts.
/// - `v1 = off` keeps the quota unleased; `v1 = shadow` observes only.
/// - `v1 = enforce` lifts only once the handoff has actually entered an overlap
///   or invocation-primary phase; a `v1 = enforce` row still parked in the
///   emergency-primary phase has not armed the overlap and stays unleased.
///
/// The caller additionally requires a matching durable fencing token before it
/// acts on a [`InvocationLiftDecision::Lift`]; this function never authorizes a
/// lift on epoch alone.
///
/// # Shadow CLAMPS — it does not speed anything up
///
/// Read this before arming `v1 = shadow` in production. Only
/// [`InvocationLiftDecision::Lift`] ever raises `cpu.max`. `Shadow` binds the
/// invocation and records telemetry (the "would throttle" arms) and then leaves
/// the leaf pinned at the broker's unleased quota —
/// `UnleasedQuota::DEFAULT_MILLICORES`, i.e. **250m** — for the whole command.
/// So a rollout that seeds the epoch and arms shadow makes every leased build
/// slower, not faster: it is an observation mode whose entire purpose is to
/// measure what enforcement *would* do. This is correct by design and asserted by
/// `shadow_epoch_binds_but_never_lifts` in `djinn-agent`'s
/// `process/tests/process_lease_tests.rs` — do not "fix" it. Only `v1 = enforce`
/// with a fully acknowledged
/// `ForwardOverlap`/`InvocationPrimary`/`RollbackOverlap` phase lifts the quota.
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
/// Production ran the launcher armed with the `admission_handoff` row ABSENT
/// (`djinn-server epoch show` → `admission handoff row: <absent>`), which is this
/// function's `Ok(None)` arm. A measured leaf reached 21,130,868 usec of CPU —
/// 84x the 250,000 usec escalation threshold — with `cpu.max` never leaving
/// `25000 100000`, making an armed launcher ~16x slower than a disabled one.
///
/// `Unleased` now selects `LeaseAuthority::Unarmed` at leaf creation (see
/// `djinn-agent`'s `process_broker::birth_authority`), so the leaf is born with no
/// quota of its own and inherits the Pod's budget. It is still contained, still
/// killable, still measurable — it just costs nothing. Asserted behaviourally on
/// a real cgroup2 hierarchy by step 7 of
/// `djinn-cgroup-launcher/tests/delegated_cpu_lease_lifecycle.rs`, and on the
/// production composition by
/// `unleased_epoch_is_born_unclamped_because_no_grant_can_ever_lift_it`.
#[must_use]
pub fn evaluate_invocation_lift(
    row: Result<Option<AdmissionHandoffRow>, ()>,
) -> InvocationLiftDecision {
    let Ok(Some(row)) = row else {
        return InvocationLiftDecision::Unleased;
    };
    // Neither authority enforces: no admission control at all. Fail closed.
    if !row.v0_mode.is_enforcing() && !row.v1_mode.is_enforcing() {
        return InvocationLiftDecision::Unleased;
    }
    // The current phase's required acknowledgements must be at the current epoch
    // before the row's modes are authoritative. Anything else is a stale epoch.
    let emergency_current = row.emergency_ack_epoch == Some(row.epoch);
    let invocation_current = row.invocation_ack_epoch == Some(row.epoch);
    let complete = match row.phase {
        AdmissionHandoffPhase::EmergencyPrimary => emergency_current,
        AdmissionHandoffPhase::ForwardOverlap | AdmissionHandoffPhase::RollbackOverlap => {
            emergency_current && invocation_current
        }
        AdmissionHandoffPhase::InvocationPrimary => invocation_current,
    };
    if !complete {
        return InvocationLiftDecision::Unleased;
    }
    match row.v1_mode {
        V1Mode::Off => InvocationLiftDecision::Unleased,
        V1Mode::Shadow => InvocationLiftDecision::Shadow,
        V1Mode::Enforce => match row.phase {
            AdmissionHandoffPhase::ForwardOverlap
            | AdmissionHandoffPhase::InvocationPrimary
            | AdmissionHandoffPhase::RollbackOverlap => InvocationLiftDecision::Lift,
            // v1 enforce configured but the overlap has not been armed yet.
            AdmissionHandoffPhase::EmergencyPrimary => InvocationLiftDecision::Unleased,
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use djinn_db::V0Mode;

    /// Arm the durable epoch to exactly the production state of goxi blocker 13:
    /// `ForwardOverlap` · v0 `Enforce` · v1 `Enforce` · both acks at the epoch.
    async fn arm_forward_overlap(db: &Database) {
        use djinn_db::AdmissionHandoffAuthority;
        let handoff = AdmissionHandoffRepository::new(db.clone());
        let row = handoff.seed_baseline().await.expect("seed the baseline");
        let row = handoff
            .set_modes_and_cap(row.epoch, V0Mode::Enforce, V1Mode::Enforce, Some(3))
            .await
            .expect("arm v1 enforcement");
        handoff
            .acknowledge(AdmissionHandoffAuthority::Emergency, row.epoch)
            .await
            .expect("emergency acknowledges the baseline");
        let row = handoff
            .advance(row.epoch, AdmissionHandoffPhase::ForwardOverlap, &[])
            .await
            .expect("enter the forward overlap");
        handoff
            .acknowledge(AdmissionHandoffAuthority::Emergency, row.epoch)
            .await
            .expect("emergency acknowledges the overlap");
        handoff
            .acknowledge(AdmissionHandoffAuthority::Invocation, row.epoch)
            .await
            .expect("invocation acknowledges the overlap");
    }

    /// The production authority, over a real database in the exact armed state
    /// `djinn-server epoch show` reported, must return `Lift`.
    #[tokio::test]
    async fn armed_forward_overlap_lifts_through_the_durable_authority() {
        let db = Database::open_in_memory().expect("ephemeral test database");
        arm_forward_overlap(&db).await;
        let authority = DurableInvocationLiftAuthority::new(db, "test");
        assert_eq!(
            authority.invocation_lift_decision().await,
            InvocationLiftDecision::Lift,
            "ForwardOverlap · v1 Enforce · both acks current is the armed state; \
             the authority that reads it must say Lift"
        );
    }

    /// The distinction `.map_err(|_| ())` destroyed, and why blocker 13 was
    /// invisible: a read that FAILED (this process cannot see the
    /// `admission_handoff` table — e.g. it was handed a project's `DATABASE_URL`
    /// catalog-service sidecar instead of `DJINN_DATABASE_URL`) is not the same
    /// event as an epoch that is legitimately unarmed, even though both correctly
    /// fail closed. Both project to `Unleased`; only one is a defect, and they must
    /// be separable at the seam that logs them.
    #[tokio::test]
    async fn a_failed_read_is_distinguishable_from_an_absent_row_and_both_fail_closed() {
        // Legitimately unarmed: the row is gone.
        let armed = Database::open_in_memory().expect("ephemeral test database");
        arm_forward_overlap(&armed).await;
        AdmissionHandoffRepository::new(armed.clone())
            .delete_for_test()
            .await
            .expect("delete the singleton");
        let absent = DurableInvocationLiftAuthority::new(armed, "test");
        assert!(
            matches!(absent.read_epoch().await, AdmissionEpochRead::Absent),
            "a deleted row is Absent, not Failed"
        );
        assert_eq!(
            absent.invocation_lift_decision().await,
            InvocationLiftDecision::Unleased
        );

        // A DEFECT: a valid, reachable Postgres that simply is not the platform
        // database. This is the shape of the wrong-DSN hazard — the maintenance
        // database on the same server has no `admission_handoff` table, exactly
        // like a task-run Pod's `svc-postgres` catalog sidecar.
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
        let read = broken.read_epoch().await;
        assert!(
            matches!(read, AdmissionEpochRead::Failed(_)),
            "a database with no admission_handoff table is a FAILED read, not an \
             unarmed epoch; got {read:?}"
        );
        assert_eq!(
            broken.invocation_lift_decision().await,
            InvocationLiftDecision::Unleased,
            "a failed read still fails closed — loudly, but closed"
        );
    }

    fn row(
        phase: AdmissionHandoffPhase,
        emergency: bool,
        invocation: bool,
        v0_mode: V0Mode,
        v1_mode: V1Mode,
    ) -> AdmissionHandoffRow {
        AdmissionHandoffRow {
            phase,
            epoch: 7,
            emergency_ack_epoch: emergency.then_some(7),
            invocation_ack_epoch: invocation.then_some(7),
            v0_mode,
            v1_mode,
            cap: None,
            updated_at: "now".into(),
        }
    }

    #[test]
    fn lifts_only_in_committed_v1_overlap_or_primary() {
        // Unreadable / missing epochs keep the quota unleased.
        assert_eq!(
            evaluate_invocation_lift(Err(())),
            InvocationLiftDecision::Unleased
        );
        assert_eq!(
            evaluate_invocation_lift(Ok(None)),
            InvocationLiftDecision::Unleased
        );

        // Baseline (v1 off) never lifts even with a complete emergency ack.
        assert_eq!(
            evaluate_invocation_lift(Ok(Some(row(
                AdmissionHandoffPhase::EmergencyPrimary,
                true,
                false,
                V0Mode::Enforce,
                V1Mode::Off,
            )))),
            InvocationLiftDecision::Unleased
        );

        // Shadow observes but never lifts.
        assert_eq!(
            evaluate_invocation_lift(Ok(Some(row(
                AdmissionHandoffPhase::EmergencyPrimary,
                true,
                false,
                V0Mode::Enforce,
                V1Mode::Shadow,
            )))),
            InvocationLiftDecision::Shadow
        );

        // v1 enforce still parked in emergency-primary has not armed the overlap.
        assert_eq!(
            evaluate_invocation_lift(Ok(Some(row(
                AdmissionHandoffPhase::EmergencyPrimary,
                true,
                false,
                V0Mode::Enforce,
                V1Mode::Enforce,
            )))),
            InvocationLiftDecision::Unleased
        );

        // A committed forward overlap with v1 enforcing lifts.
        assert_eq!(
            evaluate_invocation_lift(Ok(Some(row(
                AdmissionHandoffPhase::ForwardOverlap,
                true,
                true,
                V0Mode::Enforce,
                V1Mode::Enforce,
            )))),
            InvocationLiftDecision::Lift
        );
        // Invocation-primary lifts (v0 disabled, v1 enforcing).
        assert_eq!(
            evaluate_invocation_lift(Ok(Some(row(
                AdmissionHandoffPhase::InvocationPrimary,
                false,
                true,
                V0Mode::Disabled,
                V1Mode::Enforce,
            )))),
            InvocationLiftDecision::Lift
        );
        // Rollback overlap still has v1 enforcing, so it may still lift.
        assert_eq!(
            evaluate_invocation_lift(Ok(Some(row(
                AdmissionHandoffPhase::RollbackOverlap,
                true,
                true,
                V0Mode::Enforce,
                V1Mode::Enforce,
            )))),
            InvocationLiftDecision::Lift
        );

        // An INCOMPLETE overlap epoch (missing the invocation ack) is stale and
        // must not lift.
        assert_eq!(
            evaluate_invocation_lift(Ok(Some(row(
                AdmissionHandoffPhase::ForwardOverlap,
                true,
                false,
                V0Mode::Enforce,
                V1Mode::Enforce,
            )))),
            InvocationLiftDecision::Unleased
        );

        // The illegal both-non-enforcing combo fails closed.
        assert_eq!(
            evaluate_invocation_lift(Ok(Some(row(
                AdmissionHandoffPhase::EmergencyPrimary,
                true,
                false,
                V0Mode::Observe,
                V1Mode::Shadow,
            )))),
            InvocationLiftDecision::Unleased
        );
    }
}
