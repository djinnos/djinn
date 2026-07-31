//! The per-invocation cgroup CPU boundary, proved against the POST-cutover types.
//!
//! The Kueue cutover (proposal `9oga`) deleted the build-admission ledger. It
//! explicitly retained the per-invocation cgroup CPU lease as a non-goal, and
//! slice S3b then re-homed the arming authority onto a much smaller surface:
//! one singleton row whose `mode` is both the arming switch and the operator
//! kill switch, and whose `cap` is the reference cap the build-slot FIFO
//! enforces.
//!
//! "Retained as a non-goal" is a claim about behaviour, and nothing about the
//! cutover produces a compile error if that behaviour quietly stopped. So this
//! module asserts the five properties the boundary actually consists of, end to
//! end, against a real Postgres database:
//!
//! 1. **The cap is a ceiling.** With the authority armed at cap K and M > K
//!    invocations contending, exactly K occupy capacity.
//! 2. **The arming switch is the only input to the lift decision.** `off` and an
//!    absent row are both [`InvocationLiftDecision::Unleased`]; `shadow`
//!    observes without lifting; only `enforce` lifts.
//! 3. **The lease is per-INVOCATION, not per-pod.** Three sequential heavy
//!    commands on one bound pod take three distinct leases, and the gaps between
//!    them are real capacity that somebody else gets.
//! 4. **A returning command has no tenure.** It re-enters at the BACK of the
//!    durable FIFO.
//! 5. **The reference cap is adopted at RUNTIME.** `epoch set-cap` moves the cap
//!    the live process enforces, without a restart, and a raise drains.
//!
//! # What this module deliberately does not touch
//!
//! There is no `Fake` or `Mock` lease repository here, and no reference to the
//! physical `admission_handoff` columns except through
//! [`InvocationLeaseAuthorityRepository`]. Both restrictions are the point: the
//! boundary is a property of durable Postgres state under a real advisory lock,
//! and a double whose `grant_next` is a counter in a `Mutex` would agree with
//! every assertion below while proving none of them. Proposal `3i92` depends on
//! these same types, so their semantics are load-bearing beyond this file.
//!
//! # Why assertions here are on durable rows, never on returned enums
//!
//! The production defect of 2026-07-25 was a cap that was STORED but never
//! ADOPTED: `epoch set-cap --cap 12` reported success, `epoch show` read back
//! `cap 12`, and the live `grant_next` kept denying at `occupancy=3 cap=3`.
//! Every assertion that would have "passed" during that incident is a read-back
//! of something the process wrote down. So occupancy here is always
//! `BuildLeaseSnapshot::occupied` — the `SUM(weight)` over the occupying states
//! that `grant_next` itself compares against the cap — and never the
//! [`LeaseResult`] a call happened to return.

use std::sync::Arc;

use djinn_db::{
    BuildLeaseConsumerKind, BuildLeaseKey, BuildLeaseRepository, BuildLeaseRow, BuildLeaseState,
    Database, InvocationLeaseAuthorityRepository, InvocationLeaseMode,
};
use djinn_supervisor::services::invocation_admission::InvocationLeaseAuthorityRead;
use djinn_supervisor::services::{
    DurableInvocationLiftAuthority, InvocationLiftAuthority, InvocationLiftDecision,
    LeaseBindRequest, LeaseDeadlines, LeaseFencingToken, LeaseIdentity, LeaseQueueRequest,
    LeaseReleaseRequest, LeaseResult, TaskInvocationLeaseIdentity,
};

use crate::build_lease::BuildLeaseService;
use crate::invocation_lease_control::InvocationLeaseControl;

/// `DJINN_MAX_BUILD_TASKRUNS` stand-in. Deliberately different from — and larger
/// than — every armed cap used below, so any assertion that passes by reading
/// the process configuration instead of the durable authority is visible as a
/// wrong number rather than as a coincidence.
const CONFIGURED_FALLBACK: i64 = 9;

/// One task-run's pod. Immutable for the life of the pod, which is exactly the
/// unit a pod-scoped reservation would have keyed on.
const POD_UID: &str = "b6c0f2a4-8b1d-4f5e-9c33-6e2a7d0f1a58";

// ─── Fixture ────────────────────────────────────────────────────────────────

/// A live coordinator composition: the real durable authority, the real
/// build-lease repository, and the same `BuildLeaseService` `AppState` builds.
struct Boundary {
    service: Arc<BuildLeaseService>,
    leases: Arc<BuildLeaseRepository>,
    operator: InvocationLeaseControl,
}

/// The durable census `grant_next` decides against, read straight out of
/// Postgres.
#[derive(Debug, PartialEq, Eq)]
struct Census {
    /// `occupancy_tx`: `SUM(weight)` over `granted|launching|bound|active|suspect`.
    occupied: i64,
    /// How many rows are in one of those occupying states.
    occupying_rows: usize,
    /// How many rows are still waiting for capacity.
    queued: usize,
}

impl Boundary {
    /// Compose the process the way `AppState` does, with the durable authority
    /// armed to `mode` at reference cap `cap`.
    async fn armed(mode: InvocationLeaseMode, cap: i64) -> Self {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let authority = Arc::new(InvocationLeaseAuthorityRepository::new(db.clone()));
        let operator = InvocationLeaseControl::new(Arc::clone(&authority));
        let seeded = operator.seed().await.unwrap();
        assert_eq!(
            seeded.mode,
            InvocationLeaseMode::Off,
            "precondition: a deployment that has never been armed leases nothing"
        );
        operator.arm(seeded.epoch, mode, Some(cap)).await.unwrap();

        let leases = Arc::new(BuildLeaseRepository::new(db.clone()));
        let service = Arc::new(
            BuildLeaseService::new(Arc::clone(&leases), CONFIGURED_FALLBACK)
                .with_invocation_lease_authority(Arc::clone(&authority)),
        );
        assert!(matches!(service.recover().await, LeaseResult::Status(_)));
        assert_eq!(
            service.cap(),
            cap,
            "precondition: the armed reference cap outranks the configured fallback"
        );
        Self {
            service,
            leases,
            operator,
        }
    }

    /// Run the operator command the runbook runs — `djinn-server epoch show`
    /// followed by `epoch set-cap` — through the real control surface. Nothing
    /// here simulates the write path or touches a physical column.
    async fn operator_set_cap(&self, cap: i64) {
        let current = self.operator.show().await.unwrap().unwrap();
        self.operator.set_cap(current.epoch, cap).await.unwrap();
    }

    /// Escalate one invocation, exactly as `LeaseInvocationRunner::output` does:
    /// a fresh invocation id per command, no deadlines (this suite is about
    /// capacity, not expiry).
    async fn escalate(&self, task: &str, run: &str, invocation: &str) -> LeaseResult {
        self.service
            .queue(LeaseQueueRequest {
                identity: identity(task, run, invocation),
                deadlines: LeaseDeadlines {
                    queue_deadline_ms: 0,
                    launch_deadline_ms: 0,
                },
            })
            .await
    }

    /// Escalate and require a grant, returning the fencing token the durable
    /// row was minted with.
    async fn granted(&self, task: &str, run: &str, invocation: &str) -> LeaseFencingToken {
        match self.escalate(task, run, invocation).await {
            LeaseResult::Granted(grant) => grant.fencing_token,
            other => panic!("expected a grant for {invocation}, got {other:?}"),
        }
    }

    /// Bind the invocation lease onto the concrete pod the command runs in.
    async fn bind(&self, task: &str, run: &str, invocation: &str, token: &LeaseFencingToken) {
        let bound = self
            .service
            .bind(LeaseBindRequest {
                identity: identity(task, run, invocation),
                fencing_token: token.clone(),
                pod_uid: POD_UID.into(),
            })
            .await;
        assert!(
            matches!(bound, LeaseResult::Bound(_)),
            "the invocation must bind onto its pod: {bound:?}"
        );
    }

    /// The command ended: surrender the slot under its fencing token.
    async fn release(&self, task: &str, run: &str, invocation: &str, token: &LeaseFencingToken) {
        let released = self
            .service
            .release(LeaseReleaseRequest {
                identity: identity(task, run, invocation),
                fencing_token: token.clone(),
                candidate_cleanup: false,
            })
            .await;
        assert!(
            matches!(released, LeaseResult::Released { .. }),
            "releasing {invocation} must succeed: {released:?}"
        );
    }

    async fn row(&self, invocation: &str) -> BuildLeaseRow {
        self.leases
            .get(&BuildLeaseKey {
                consumer_kind: BuildLeaseConsumerKind::TaskInvocation,
                consumer_id: invocation.into(),
            })
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("no durable row for {invocation}"))
    }

    async fn state(&self, invocation: &str) -> BuildLeaseState {
        self.row(invocation).await.state
    }

    /// The fencing token minted for a row that was granted by a drain rather
    /// than by its own `queue` call.
    async fn token(&self, invocation: &str) -> LeaseFencingToken {
        LeaseFencingToken(
            self.row(invocation)
                .await
                .fencing_token
                .unwrap_or_else(|| panic!("{invocation} holds no fencing token"))
                as u64,
        )
    }

    async fn census(&self) -> Census {
        let snapshot = self.leases.snapshot().await.unwrap();
        let occupying_rows = snapshot
            .rows
            .iter()
            .filter(|row| !matches!(row.state, BuildLeaseState::Queued))
            .count();
        let queued = snapshot
            .rows
            .iter()
            .filter(|row| row.state == BuildLeaseState::Queued)
            .count();
        Census {
            occupied: snapshot.occupied,
            occupying_rows,
            queued,
        }
    }
}

fn identity(task: &str, run: &str, invocation: &str) -> LeaseIdentity {
    LeaseIdentity::TaskInvocation(TaskInvocationLeaseIdentity {
        task_id: task.into(),
        task_run_id: run.into(),
        invocation_id: invocation.into(),
    })
}

// ─── AC1: the reference cap is a ceiling ────────────────────────────────────

/// **The cap enforces.** With the authority armed at cap K and M > K
/// invocations contending at once, exactly K occupy capacity and the rest wait.
///
/// The assertion is on `SUM(weight)` over the occupying states — the identical
/// expression `grant_next` compares against the cap inside its own transaction
/// — and on the durable state of every row. It is deliberately NOT on the
/// [`LeaseResult`] each call returned: a returned enum is what the caller was
/// told, and the 2026-07-25 incident was precisely a system that told callers
/// one thing and enforced another.
///
/// The contenders are genuinely concurrent (`M` spawned tasks against one
/// `Arc<BuildLeaseService>`), so the ceiling is proved against the repository's
/// advisory lock rather than against a sequence the test chose.
///
/// Non-vacuity is the second half: raising the reference cap to M through the
/// real operator surface and adopting it at runtime must grant ALL M. Without
/// that, a `grant_next` that had simply stopped granting anything would satisfy
/// the first half perfectly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exactly_the_reference_cap_of_contending_invocations_occupies_capacity() {
    const CAP: i64 = 2;
    const CONTENDERS: usize = 5;

    let boundary = Arc::new(Boundary::armed(InvocationLeaseMode::Enforce, CAP).await);

    let mut escalations = Vec::new();
    for index in 0..CONTENDERS {
        let boundary = Arc::clone(&boundary);
        escalations.push(tokio::spawn(async move {
            boundary
                .escalate(
                    &format!("task-{index}"),
                    &format!("run-{index}"),
                    &format!("invocation-{index}"),
                )
                .await
        }));
    }
    for escalation in escalations {
        escalation.await.unwrap();
    }

    assert_eq!(
        boundary.census().await,
        Census {
            occupied: CAP,
            occupying_rows: CAP as usize,
            queued: CONTENDERS - CAP as usize,
        },
        "with the authority armed at cap {CAP}, exactly {CAP} of {CONTENDERS} \
         contending invocations may occupy build capacity"
    );

    // Non-vacuity: the queue is not simply dead. Raise the reference cap
    // through the operator surface and adopt it, and every contender grants.
    boundary.operator_set_cap(CONTENDERS as i64).await;
    assert_eq!(
        boundary.service.refresh_epoch_cap().await,
        Some(CONTENDERS as i64)
    );
    assert_eq!(
        boundary.census().await,
        Census {
            occupied: CONTENDERS as i64,
            occupying_rows: CONTENDERS,
            queued: 0,
        },
        "at cap {CONTENDERS} every one of the {CONTENDERS} contenders occupies \
         capacity, so the ceiling above was the cap and not a stuck queue"
    );
}

// ─── AC2: the arming switch is the only input to the lift decision ──────────

/// **Only an enforcing authority lifts `cpu.max`.**
///
/// `Unleased` is not a cosmetic outcome: it selects `LeaseAuthority::Unarmed` at
/// leaf creation, which removes the per-invocation CPU boundary altogether. So
/// each arm is asserted against a real durable row read back through the
/// production reader, over real Postgres — never against a hand-built
/// [`InvocationLeaseAuthorityRow`].
///
/// The `enforce` arm is the non-vacuity control: without it, a reader hard-wired
/// to `Unleased` would satisfy every other assertion here.
///
/// "Shadow WITHOUT lifting" is asserted twice, because the launcher-facing enum
/// alone cannot show that shadow does not enforce: the decision must be
/// `Shadow` and not `Lift`, AND the live `BuildLeaseService` that adopted the
/// same row must report that it is not enforcing.
#[tokio::test]
async fn off_and_an_absent_row_are_both_unleased_while_only_enforce_lifts() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let authority = Arc::new(InvocationLeaseAuthorityRepository::new(db.clone()));
    let operator = InvocationLeaseControl::new(Arc::clone(&authority));
    let reader = DurableInvocationLiftAuthority::new(db.clone(), "pkg0-invocation-cpu-boundary");

    // The same live process observes each mode change, so the arming switch is
    // read at runtime rather than at startup.
    let service = Arc::new(
        BuildLeaseService::new(
            Arc::new(BuildLeaseRepository::new(db.clone())),
            CONFIGURED_FALLBACK,
        )
        .with_invocation_lease_authority(Arc::clone(&authority)),
    );
    assert!(matches!(service.recover().await, LeaseResult::Status(_)));

    let baseline = operator.seed().await.unwrap();

    // `enforce` — the control arm. This is the mode production runs.
    let armed = operator
        .arm(baseline.epoch, InvocationLeaseMode::Enforce, Some(3))
        .await
        .unwrap();
    assert_eq!(
        reader.invocation_lift_decision().await,
        InvocationLiftDecision::Lift,
        "an armed authority must lift the per-invocation cgroup CPU quota"
    );
    service.refresh_epoch_cap().await;
    assert!(
        service.dispatch_enforcing(),
        "and the live service must observe the same row as enforcing"
    );

    // `shadow` — observes what enforcement would do, and lifts nothing.
    let shadowing = operator
        .arm(armed.epoch, InvocationLeaseMode::Shadow, None)
        .await
        .unwrap();
    let decision = reader.invocation_lift_decision().await;
    assert_eq!(decision, InvocationLiftDecision::Shadow);
    assert_ne!(
        decision,
        InvocationLiftDecision::Lift,
        "shadow CLAMPS: it binds and measures, and never raises cpu.max"
    );
    service.refresh_epoch_cap().await;
    assert!(
        !service.dispatch_enforcing(),
        "a shadowing authority is not an enforcing one"
    );

    // `off` — the operator kill switch.
    let disarmed = operator.kill_switch(shadowing.epoch).await.unwrap();
    assert_eq!(disarmed.mode, InvocationLeaseMode::Off);
    assert_eq!(
        reader.invocation_lift_decision().await,
        InvocationLiftDecision::Unleased,
        "the kill switch must disarm the lease"
    );

    // An absent row — the documented state of a deployment that has never armed
    // the authority, and the documented remediation for a wedged one. It must
    // reach `Unleased` by being ABSENT, not by the read failing: those are
    // different events and only one of them is a defect.
    authority.delete_for_test().await.unwrap();
    let read = reader.read_authority().await;
    assert!(
        matches!(read, InvocationLeaseAuthorityRead::Absent),
        "the deleted row must read as Absent, not Failed; got {read:?}"
    );
    assert_eq!(
        reader.invocation_lift_decision().await,
        InvocationLiftDecision::Unleased,
        "a missing authority fails closed to Unleased"
    );
}

// ─── AC3: the lease is per-invocation, not per-pod ──────────────────────────

/// **Three sequential heavy commands on ONE bound pod take THREE leases.**
///
/// This is the design property the whole layer rests on, and it is invisible in
/// the type system: `LeaseInvocationRunner::output` mints a fresh
/// `uuid::Uuid::now_v7()` invocation id per command, so the durable key is the
/// *command*, not the pod. A pod-scoped reservation would be a perfectly
/// sensible-looking alternative — and it would silently hold a build slot
/// across every `git status` and every model round-trip between two compiles.
///
/// The assertion that separates the two models is not the row count; it is the
/// GAP. At cap 1, an unrelated task's invocation must be refused while each
/// command holds the slot and granted the moment that command ends. Under a
/// pod-scoped reservation there are no gaps at all, because the pod never lets
/// go between commands.
///
/// Verified by mutation: replacing the per-command lease with ONE pod-scoped
/// row (`granted(TASK, RUN, POD_UID)` hoisted above the loop, no release
/// between commands) fails on the gap assertion below with `left: Queued /
/// right: Granted`.
#[tokio::test]
async fn three_sequential_commands_on_one_pod_take_three_distinct_invocation_leases() {
    const TASK: &str = "task-under-test";
    const RUN: &str = "task-run-under-test";
    const RIVAL: &str = "unrelated-task";

    let boundary = Boundary::armed(InvocationLeaseMode::Enforce, 1).await;
    let mut invocations = Vec::new();

    for command in 0..3 {
        // One heavy command. Production derives this id from a v7 UUID; the
        // only property that matters is that it is per-command, so it is
        // spelled out here rather than generated.
        let invocation = format!("invocation-{command}");
        let token = boundary.granted(TASK, RUN, &invocation).await;
        boundary.bind(TASK, RUN, &invocation, &token).await;
        assert_eq!(boundary.census().await.occupied, 1);

        // A rival escalation is refused for as long as this command holds the
        // only slot.
        let rival = format!("rival-invocation-{command}");
        boundary.escalate(RIVAL, "rival-run", &rival).await;
        assert_eq!(
            boundary.state(&rival).await,
            BuildLeaseState::Queued,
            "command {command} holds the only build slot, so the rival waits"
        );

        // The command ends. THE GAP: the slot is genuinely free between two
        // commands of the same pod, and the rival takes it.
        boundary.release(TASK, RUN, &invocation, &token).await;
        assert_eq!(
            boundary.state(&rival).await,
            BuildLeaseState::Granted,
            "the gap between two commands on ONE pod is real, releasable \
             capacity; a pod-scoped reservation would hold it across the gap \
             and the rival would still be queued here"
        );

        // Hand the slot back so the next command of the same pod has to buy it
        // again — which is the other half of "per invocation".
        let rival_token = boundary.token(&rival).await;
        boundary
            .release(RIVAL, "rival-run", &rival, &rival_token)
            .await;
        invocations.push((invocation, token));
    }

    // Three distinct durable `task_invocation` rows, three distinct invocation
    // ids, three distinct fencing tokens, all bound to the same one pod.
    let bound = boundary
        .leases
        .list_pod_bound_task_invocations()
        .await
        .unwrap();
    assert_eq!(
        bound.len(),
        3,
        "three commands on one pod must leave three durable invocation rows, \
         not one pod-scoped row reused three times"
    );
    let ids: Vec<&str> = bound
        .iter()
        .map(|row| row.key.consumer_id.as_str())
        .collect();
    let mut unique = ids.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique.len(),
        3,
        "the three invocation ids are distinct: {ids:?}"
    );
    for row in &bound {
        assert_eq!(
            row.bound_pod_uid.as_deref(),
            Some(POD_UID),
            "every one of the three leases was bound to the same pod"
        );
    }
    let mut tokens: Vec<u64> = invocations.iter().map(|(_, token)| token.0).collect();
    tokens.sort_unstable();
    tokens.dedup();
    assert_eq!(
        tokens.len(),
        3,
        "each command was fenced under its own never-reissued token"
    );
}

// ─── AC4: a returning command re-enters at the back of the queue ────────────

/// **A returning command has no tenure.**
///
/// Having just held the only build slot buys a task-run nothing: its next
/// command is a new arrival at the back of the durable FIFO, behind everyone
/// who was already waiting. The alternative — letting the previous holder cut
/// back in — is what turns one busy pod into indefinite starvation for every
/// other consumer sharing the single cap.
///
/// The assertion is on the durable `enqueue_sequence` values, not on the order
/// the test happened to make its calls: `enqueue_sequence` is the `BIGSERIAL`
/// that `grant_next` orders by, so it is the only value that decides who is
/// next. The FIFO is then walked to the end, which is what rules out "A is
/// merely behind B" and proves it is behind ALL of them.
#[tokio::test]
async fn a_returning_command_re_enters_at_the_back_of_the_durable_queue() {
    const TASK_A: &str = "task-a";

    let boundary = Boundary::armed(InvocationLeaseMode::Enforce, 1).await;

    // A holds the only slot; B and C queue behind it.
    let first = boundary.granted(TASK_A, "run-a", "a-command-1").await;
    boundary.escalate("task-b", "run-b", "b-command").await;
    boundary.escalate("task-c", "run-c", "c-command").await;
    let sequence_b = boundary.row("b-command").await.enqueue_sequence;
    let sequence_c = boundary.row("c-command").await.enqueue_sequence;
    assert!(
        sequence_b < sequence_c,
        "precondition: B queued before C ({sequence_b} < {sequence_c})"
    );

    // A's command ends and A immediately issues its next heavy command.
    boundary
        .release(TASK_A, "run-a", "a-command-1", &first)
        .await;
    boundary.escalate(TASK_A, "run-a", "a-command-2").await;

    let sequence_next = boundary.row("a-command-2").await.enqueue_sequence;
    assert!(
        sequence_next > sequence_b && sequence_next > sequence_c,
        "A's returning command must enqueue BEHIND both waiters \
         (a-command-2={sequence_next}, b={sequence_b}, c={sequence_c})"
    );
    assert_eq!(
        boundary.state("b-command").await,
        BuildLeaseState::Granted,
        "B is granted before A's next command runs"
    );
    assert_eq!(
        boundary.state("a-command-2").await,
        BuildLeaseState::Queued,
        "and A's next command waits, despite having held the slot a moment ago"
    );
    assert_eq!(boundary.state("c-command").await, BuildLeaseState::Queued);

    // Walk the FIFO to the end: C — not the returning command — is next.
    let token_b = boundary.token("b-command").await;
    boundary
        .release("task-b", "run-b", "b-command", &token_b)
        .await;
    assert_eq!(boundary.state("c-command").await, BuildLeaseState::Granted);
    assert_eq!(
        boundary.state("a-command-2").await,
        BuildLeaseState::Queued,
        "A is behind EVERY waiter, not merely behind the first one"
    );

    let token_c = boundary.token("c-command").await;
    boundary
        .release("task-c", "run-c", "c-command", &token_c)
        .await;
    assert_eq!(
        boundary.state("a-command-2").await,
        BuildLeaseState::Granted,
        "the queue does reach A again — the boundary delays it, it does not \
         starve it"
    );
}

// ─── AC5: the reference cap is adopted at runtime ───────────────────────────

/// **`epoch set-cap` moves what the RUNNING process enforces.**
///
/// Production, 2026-07-25, mid-incident: `djinn-server epoch set-cap --cap 12`
/// reported `set-cap: applied` and `epoch show` read back `cap 12`, while every
/// subsequent denial still said `occupancy=3 cap=3`. The durable write landed;
/// the live `grant_next` read a cached atomic that only a restart refreshed.
///
/// So the assertion here is a GRANT COUNT — invocations that were refused at
/// the old cap becoming occupants of real capacity — measured on the same
/// `Arc<BuildLeaseService>` that was recovered once at the top of the test and
/// never recovered again. Reading the durable cap back is exactly the check
/// that passed throughout the incident, and it is not what this asserts.
///
/// Verified by mutation: deleting the [`BuildLeaseService::adopt_authority_cap`]
/// call from `refresh_epoch_cap` reproduces the incident exactly — the post-
/// `set_cap` census stays at `occupied: 1, occupying_rows: 1, queued: 2` while
/// the durable row reads 3.
#[tokio::test]
async fn refresh_epoch_cap_adopts_a_new_reference_cap_without_a_restart() {
    let boundary = Boundary::armed(InvocationLeaseMode::Enforce, 1).await;

    let holder = boundary
        .granted("task-holder", "run-holder", "holder")
        .await;
    boundary
        .escalate("task-waiter-1", "run-1", "waiter-1")
        .await;
    boundary
        .escalate("task-waiter-2", "run-2", "waiter-2")
        .await;
    assert_eq!(
        boundary.census().await,
        Census {
            occupied: 1,
            occupying_rows: 1,
            queued: 2,
        },
        "precondition: cap 1 is genuinely enforced before the operator acts"
    );

    // The incident action. It succeeds durably, and on its own it changes
    // nothing about what this process enforces — which is the whole defect.
    boundary.operator_set_cap(3).await;
    assert_eq!(
        boundary.census().await.occupying_rows,
        1,
        "a durable write alone must not be mistaken for an adopted cap"
    );
    assert_eq!(boundary.service.cap(), 1);

    // The adoption. Same process, same service, no restart.
    let adopted = boundary.service.refresh_epoch_cap().await;
    assert_eq!(
        boundary.census().await,
        Census {
            occupied: 3,
            occupying_rows: 3,
            queued: 0,
        },
        "the raised reference cap must be ADOPTED at runtime and must DRAIN: \
         both invocations refused at cap 1 now occupy real capacity"
    );
    assert_eq!(adopted, Some(3));
    assert_eq!(boundary.service.cap(), 3);

    // The pre-existing occupant was never disturbed by the cap change: its
    // fencing token still fences, so this was an adoption and not a recovery.
    boundary
        .release("task-holder", "run-holder", "holder", &holder)
        .await;
    assert_eq!(boundary.census().await.occupying_rows, 2);
}
