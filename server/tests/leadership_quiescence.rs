//! Shutdown ordering for the coordinator advisory lock (proposal `nafu`,
//! wave 3), proven against real Postgres.
//!
//! # The property
//!
//! `run_with_leadership` must close the provider-action gate and **join** the
//! in-flight provider futures *before* it releases the advisory lock. The lock
//! is the exclusion authority for `calling` rows: a lock released while a
//! provider future is still alive lets a new pod acquire it, see a charged
//! `calling` row, and find nothing that says "do not touch". That is the one
//! ordering `recover_calling_owner` cannot defend against, because it trusts
//! lock ownership.
//!
//! # Why this file exists at all
//!
//! Until it did, deleting the `quiesce_provider_actions(...)` call from the
//! cancellation arm of `run_with_leadership` left every test that calls
//! `run_with_leadership` green. The only such suite
//! (`task_run_resize_recovery`) passes `None` for the scope, so
//! `quiesce_provider_actions` early-returns and the drain branch had never
//! executed under test. Both tests below pass a REAL `ProviderActionScope`.
//!
//! # Why the assertion is lock AVAILABILITY, not "release was called"
//!
//! There is no callback seam on the release: `advisory_lock::release`'s result
//! is discarded, and the session-scoped lock *also* drops implicitly when the
//! dedicated connection is dropped at return. So deleting the release call
//! still releases the lock a moment later, and an assertion about the call
//! would be an assertion about a line of code rather than about exclusion.
//! What a second pod can actually observe is whether
//! `pg_try_advisory_lock(classid, objid)` succeeds — so that is what these
//! tests observe, from their own `PgConnection`, keyed on the PRODUCTION
//! [`LOCK_CLASSID`]/[`LOCK_OBJID`] constants rather than on copied literals.
//!
//! # Why a stand-in coordinator task is mandatory
//!
//! Leadership waits on `wait_until_drained` — the drain *stamp* — not on
//! emptiness, and in production the COORDINATOR writes that stamp
//! (`djinn-coordinator/src/pr_poller/ci_routing/quiescence.rs`, reached from
//! the actor's cancellation arm). That producer has its own coverage in the
//! coordinator's `ci_routing` suite, which proves the stamp is withheld until
//! the join completes; this file proves only that leadership does not move the
//! lock before the stamp exists. With no coordinator, passing
//! `Some(scope)` would make shutdown block the full 45-second
//! `PROVIDER_ACTION_DRAIN_WAIT` and then release the lock *without* a proof —
//! a slow false-green that asserts nothing. Each test therefore spawns a
//! stand-in that reproduces the coordinator's contract, strictly in order:
//! observe admission closed → `wait_until_empty` → `mark_drained`. Anything
//! else trips `mark_drained`'s debug assertions.
//!
//! Real time is used deliberately: `tokio::time::pause()` would fast-forward
//! straight through the 45-second budget and destroy the property under test,
//! and there is real Postgres I/O here anyway.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use djinn_db::{Database, advisory_lock};
use djinn_orchestration_types::ProviderActionScope;
use djinn_server::leadership::{LOCK_CLASSID, LOCK_OBJID, run_with_leadership};
use sqlx::Connection;
use sqlx::postgres::PgConnection;
use tokio_util::sync::CancellationToken;

/// How long a poll loop waits before declaring the system wedged. Generous:
/// every transition it waits on is sub-millisecond in the passing case, so a
/// long ceiling costs nothing and removes the only source of flake.
const PATIENCE: Duration = Duration::from_secs(30);

/// The window in which a mutation that drops the quiesce call is caught, as a
/// fixed sample COUNT rather than a wall-clock deadline: a slow machine must
/// make the window longer, never thinner. One check would be a race the
/// mutation could win.
const HELD_SAMPLES: u32 = 20;
const HELD_POLL: Duration = Duration::from_millis(50);

/// The stand-in for the coordinator's half of the drain contract.
///
/// Production order, preserved exactly: the coordinator only stamps after it
/// has seen admission close and joined its own futures. Stamping earlier would
/// panic in a debug build, and — worse — would make these tests pass without a
/// join ever happening.
fn spawn_stand_in_coordinator(scope: ProviderActionScope) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while !scope.is_admission_closed() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        scope.wait_until_empty().await;
        scope.mark_drained();
    })
}

/// Poll `probe` for the coordinator lock until it wins or `PATIENCE` runs out.
///
/// Note that a *successful* `try_acquire` leaves the lock held by `probe`,
/// which is exactly the state a promoting pod would be in.
async fn wait_until_lock_is_free(probe: &mut PgConnection) -> bool {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        if advisory_lock::try_acquire(probe, LOCK_CLASSID, LOCK_OBJID)
            .await
            .expect("probing the advisory lock must not error")
        {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_until_true(flag: &AtomicBool, what: &str) {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    while !flag.load(Ordering::SeqCst) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The lock stays held for as long as a provider action is in flight, and only
/// becomes acquirable after the scope reports a stamped drain.
///
/// NAMED FAILING MUTATION: delete
/// `quiesce_provider_actions(provider_action_scope.as_ref()).await;` from the
/// `cancel.cancelled()` arm of `run_with_leadership`. Cancellation then reaches
/// the release in the same breath, the probe connection wins the lock while
/// `guard` is still alive, and the held-sample loop below fails at sample 0.
/// The final `counts()` assertion fails independently for a second reason:
/// nothing ever closed admission, so the stand-in never stamped.
///
/// Multi-threaded on purpose — a leadership future, a stand-in coordinator, and
/// a polling loop with blocking-ish Postgres round trips all run concurrently,
/// and on a current-thread runtime they contend for one thread.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn the_advisory_lock_is_held_until_the_provider_scope_reports_a_stamped_drain() {
    // The owning handle must outlive the test: dropping the last one DROPs the
    // template-cloned database out from under every connection.
    let owner = Database::ephemeral().await.expect("ephemeral db");
    // The per-test database is cloned from the template LAZILY, on first use.
    // Every other suite trips that by seeding rows; this one never touches a
    // repository, so without this the DSN names a database that does not exist
    // and leadership silently retries its connection until the test times out.
    owner
        .ensure_initialized()
        .await
        .expect("materialize the per-test database");
    let dsn = owner
        .test_dsn()
        .expect("the ephemeral harness must expose its DSN");

    // Opened before leadership so a connection fault is reported as itself
    // rather than as a leadership timeout.
    let mut probe = PgConnection::connect(&dsn)
        .await
        .expect("a second session for the lock probe");

    let scope = ProviderActionScope::new();
    let cancel = CancellationToken::new();
    let promoted = Arc::new(AtomicBool::new(false));

    let leadership = tokio::spawn({
        let dsn = dsn.clone();
        let cancel = cancel.clone();
        let scope = scope.clone();
        let promoted = Arc::clone(&promoted);
        async move {
            run_with_leadership(Some(dsn), cancel, Some(scope), || async move {
                promoted.store(true, Ordering::SeqCst);
            })
            .await;
        }
    });

    wait_until_true(&promoted, "leadership to acquire the coordinator lock").await;

    // A real provider action, held across cancellation. This is the future that
    // must be joined before the lock may move.
    let guard = scope.admit().expect("an open scope admits");
    assert_eq!(scope.in_flight(), 1);

    // Sanity: without this, a test that never observed the lock held would call
    // any later "free" observation a pass.
    assert!(
        !advisory_lock::try_acquire(&mut probe, LOCK_CLASSID, LOCK_OBJID)
            .await
            .expect("probe query"),
        "the leader must hold the coordinator lock before cancellation"
    );

    let stand_in = spawn_stand_in_coordinator(scope.clone());

    cancel.cancel();

    // ── The ordering assertion. ──
    // While the guard is alive the scope cannot be empty, so the coordinator
    // cannot have stamped, so leadership must still be inside
    // `quiesce_provider_actions` and the lock must still be ours.
    for sample in 0..HELD_SAMPLES {
        tokio::time::sleep(HELD_POLL).await;
        assert!(
            !advisory_lock::try_acquire(&mut probe, LOCK_CLASSID, LOCK_OBJID)
                .await
                .expect("probe query"),
            "sample {sample}: the coordinator advisory lock became acquirable while a \
             provider action was still in flight (in_flight={}, counts={:?}); cancellation \
             released the lock without joining, which is precisely the window \
             `recover_calling_owner` cannot defend against",
            scope.in_flight(),
            scope.counts()
        );
    }
    assert!(
        !leadership.is_finished(),
        "leadership must not have returned while a provider action is in flight"
    );
    // Admission is closed *by leadership*, belt-and-braces with the coordinator:
    // a wedged or absent coordinator must still not let a new action in.
    assert!(
        scope.is_admission_closed(),
        "cancellation must close admission even before the join completes"
    );
    assert!(
        scope.admit().is_none(),
        "a quiescing scope must refuse new provider actions"
    );

    // ── Release the action; the stand-in joins and stamps. ──
    drop(guard);

    assert!(
        wait_until_lock_is_free(&mut probe).await,
        "the coordinator lock must become acquirable once the scope has drained; \
         counts={:?}",
        scope.counts()
    );

    let counts = scope.counts();
    assert!(
        counts.admission_closed,
        "the lock moved with admission still open: {counts:?}"
    );
    assert_eq!(
        counts.in_flight, 0,
        "the lock moved with provider futures still in flight: {counts:?}"
    );
    assert!(
        counts.drained,
        "the lock moved without a drain stamp — a new leader would have no \
         `provider_actions_drained_at` to trust: {counts:?}"
    );
    assert_eq!(counts.refused_total, 1, "the post-close admit was refused");

    tokio::time::timeout(PATIENCE, leadership)
        .await
        .expect("leadership must return after releasing the lock")
        .expect("the leadership task must not panic");
    tokio::time::timeout(PATIENCE, stand_in)
        .await
        .expect("the stand-in coordinator must finish")
        .expect("the stand-in must not panic");

    drop(owner);
}

/// The no-DSN branch holds the same contract, minus the lock.
///
/// `dsn == None` is the single-process / non-Kubernetes path, and it has its
/// own `quiesce_provider_actions` call. Nothing there is waiting on a proof —
/// but the gate must still close, or a route admitted during shutdown leaves a
/// charged `calling` row behind with no finalizer.
///
/// NAMED FAILING MUTATION: delete the `quiesce_provider_actions(...)` call from
/// the `let Some(dsn) = dsn else { .. }` arm. `run_with_leadership` then returns
/// the instant the token is cancelled, with the guard still alive, and the
/// `!leadership.is_finished()` assertion below fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn the_no_lock_branch_still_closes_admission_and_joins_before_returning() {
    let scope = ProviderActionScope::new();
    let cancel = CancellationToken::new();
    let promoted = Arc::new(AtomicBool::new(false));

    let leadership = tokio::spawn({
        let cancel = cancel.clone();
        let scope = scope.clone();
        let promoted = Arc::clone(&promoted);
        async move {
            run_with_leadership(None, cancel, Some(scope), || async move {
                promoted.store(true, Ordering::SeqCst);
            })
            .await;
        }
    });

    wait_until_true(&promoted, "the sole-leader branch to run `on_acquire`").await;

    let guard = scope.admit().expect("an open scope admits");
    let stand_in = spawn_stand_in_coordinator(scope.clone());

    cancel.cancel();

    // Returning here would end the process's shutdown with a provider future
    // still live. Sampled over a window rather than once, so the mutation
    // cannot win the race.
    for sample in 0..HELD_SAMPLES {
        tokio::time::sleep(HELD_POLL).await;
        assert!(
            !leadership.is_finished(),
            "sample {sample}: the sole-leader branch returned while a provider action was \
             still in flight: {:?}",
            scope.counts()
        );
    }
    assert!(
        scope.is_admission_closed(),
        "the sole-leader branch must close the gate on cancellation"
    );
    assert!(scope.admit().is_none());

    drop(guard);

    tokio::time::timeout(PATIENCE, leadership)
        .await
        .expect("the sole-leader branch must return once the scope drains")
        .expect("the leadership task must not panic");
    tokio::time::timeout(PATIENCE, stand_in)
        .await
        .expect("the stand-in coordinator must finish")
        .expect("the stand-in must not panic");

    let counts = scope.counts();
    assert!(counts.admission_closed, "{counts:?}");
    assert_eq!(counts.in_flight, 0, "{counts:?}");
    assert!(counts.drained, "{counts:?}");
}

// ---------------------------------------------------------------------------
// The composition site: one scope, two waiters
// ---------------------------------------------------------------------------

/// One Rust source with its `//` line comments removed.
///
/// The guard below matches on *code*. Without this, a comment that merely names
/// the token under guard would satisfy the assertion the guard exists to make.
/// Quote- and escape-aware, so a `//` inside a string literal is left alone
/// rather than truncating the line it sits on.
fn strip_line_comments(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    for line in source.lines() {
        let bytes = line.as_bytes();
        let mut quoted = false;
        let mut index = 0usize;
        let mut end = line.len();
        while index < bytes.len() {
            match bytes[index] {
                b'\\' if quoted => index += 1,
                b'"' => quoted = !quoted,
                b'/' if !quoted && bytes.get(index + 1) == Some(&b'/') => {
                    end = index;
                    break;
                }
                _ => {}
            }
            index += 1;
        }
        out.push_str(&line[..end]);
        out.push('\n');
    }
    out
}

/// Leadership and the coordinator are handed the **same** scope object.
///
/// Both tests above pass a scope they constructed themselves, so they prove the
/// ordering contract for *whatever* scope leadership is given. Neither can
/// prove that the scope leadership waits on is the one the coordinator admits
/// its provider actions into — and if it is not, every assertion in this file
/// still passes while the property they exist to protect is gone.
///
/// The failure is silent by construction. `CoordinatorDeps::new` seeds a
/// **private** `ProviderActionScope::new()` (see the field comment: "off-server
/// and test contexts get a private scope from `Default`"), so deleting
/// `.with_provider_action_scope(..)` from the builder chain does not fail to
/// compile and does not log. It gives the coordinator a second, disjoint scope:
/// `rerun_failed_jobs` futures are admitted into that one, leadership's
/// `wait_until_drained` watches an empty one, reports a graceful drain
/// immediately, and releases the advisory lock while a provider future from
/// this incarnation is still alive. That is the exact ordering
/// `recover_calling_owner` cannot defend against, because it trusts lock
/// ownership.
///
/// SOURCE-LEVEL, and honestly labelled. A behavioural witness would have to
/// spawn the whole agent stack (`AppState::initialize_agents`) and then read the
/// coordinator's scope back — and `CoordinatorHandle` exposes no accessor for
/// it. Adding one so this test could exist would be inventing the seam the test
/// claims to witness, and the alternative — rebuilding the deps chain here — is
/// a fixture performing the composition it is meant to observe. What is
/// checkable without inventing anything is that both consumers name the same
/// field, and that is what this asserts.
///
/// Comments are stripped before matching, so a comment naming the field
/// satisfies nothing here.
///
/// NAMED FAILING MUTATIONS.
/// (a) Delete `.with_provider_action_scope(self.inner.provider_action_scope.clone())`
///     from the `CoordinatorDeps` builder chain in `AppState::initialize_agents`:
///     the first assertion fails.
/// (b) Hand the coordinator a fresh scope
///     (`.with_provider_action_scope(ProviderActionScope::new())`): the same
///     assertion fails, because the argument is no longer the shared field.
/// (c) Make `AppState::provider_action_scope()` return a fresh scope rather
///     than `self.inner.provider_action_scope.clone()`: the accessor assertion
///     fails — leadership would otherwise wait on a scope with no producer at
///     all.
/// (d) Pass `None` for the scope in `main.rs`, or bind `leader_action_scope`
///     from anything other than `state.provider_action_scope()`: the two
///     `main.rs` assertions fail, and `quiesce_provider_actions` would
///     early-return on every shutdown — the pre-`nafu` behaviour, with the lock
///     released unconditionally.
/// (e) Replace the body of the djinn-agent facade's
///     `CoordinatorDeps::with_provider_action_scope` with `let _ = scope; self`,
///     or change `spawn_coordinator` to build a fresh inner deps: the last two
///     assertions fail. Every assertion before them reads `server/src/…` only,
///     so the scope reaching `AppState`'s builder call proves nothing about the
///     scope reaching the actor — the adapter is one crate further in, and its
///     private `inner` is why deleting the forward is silent.
#[test]
fn the_leader_scope_and_the_coordinator_scope_are_one_object() {
    const FIELD: &str = "self.inner.provider_action_scope.clone()";

    let state = strip_line_comments(include_str!("../src/server/state/mod.rs"));

    assert!(
        state.contains(&format!(".with_provider_action_scope({FIELD})")),
        "the coordinator must be built with the SAME scope leadership waits on; \
         `CoordinatorDeps::new` otherwise seeds a private one and the two \
         waiters watch disjoint sets of futures",
    );
    let accessor = state
        .find("pub fn provider_action_scope(&self)")
        .expect("`AppState` must expose the scope leadership is handed");
    let body_end = accessor
        + state[accessor..]
            .find("\n    }")
            .expect("the accessor must have a body");
    assert!(
        state[accessor..body_end].contains(FIELD),
        "the accessor leadership is handed must return that same field, not a \
         fresh scope",
    );

    assert_eq!(
        state.matches("ProviderActionScope::new()").count(),
        1,
        "`AppState` must construct exactly one scope; a second `new()` anywhere \
         in this file is a second, disjoint set of provider futures",
    );
    assert!(
        state.contains(
            "provider_action_scope: djinn_orchestration_types::ProviderActionScope::new(),"
        ),
        "and the one construction must be the field initialiser, so everything \
         downstream clones it rather than minting its own",
    );

    // ── And `main.rs` hands it to leadership rather than `None` ─────────────
    let main = strip_line_comments(include_str!("../src/main.rs"));

    assert!(
        main.contains("let leader_action_scope = state.provider_action_scope();"),
        "the leader scope must come from `AppState`, which is what makes it the \
         coordinator's",
    );
    assert!(
        main.contains("Some(leader_action_scope),"),
        "and it must be handed to `run_with_leadership`; `None` makes \
         `quiesce_provider_actions` a no-op on every shutdown path",
    );

    // ── And the djinn-agent facade actually FORWARDS it ────────────────────
    //
    // `AppState` builds `djinn_agent::actors::coordinator::CoordinatorDeps`,
    // not `djinn_coordinator`'s. Everything above proves the scope reaches that
    // adapter; nothing above proves the adapter passes it on. The adapter's
    // `with_provider_action_scope` is a one-line forwarder whose body can be
    // replaced with `let _ = scope; self` with every assertion in this file —
    // and every other `nafu` acceptance command — still green, because
    // `djinn_coordinator::CoordinatorDeps::new` seeds a private
    // `ProviderActionScope::new()` the coordinator then keeps.
    //
    // The behavioural witness for that hop lives where it can observe the
    // object: `djinn_agent::actors::coordinator::ci_routing_scope_handoff_tests`
    // admits an action through the caller's handle and reads `in_flight()` off
    // the forwarded one. This is the cheap cross-check that the chain from
    // `AppState` to the spawned actor has no other gap.
    let agent = strip_line_comments(include_str!(
        "../crates/djinn-agent/src/actors/coordinator/mod.rs"
    ));

    let forwarder = agent
        .find("pub fn with_provider_action_scope(")
        .expect("the djinn-agent coordinator facade must expose the forwarder");
    let forwarder_end = forwarder
        + agent[forwarder..]
            .find("\n    }")
            .expect("the forwarder must have a body");
    assert!(
        agent[forwarder..forwarder_end]
            .contains("self.inner = self.inner.with_provider_action_scope(scope);"),
        "the djinn-agent facade must FORWARD the scope to the inner deps; \
         dropping the argument leaves the coordinator on the private scope \
         `CoordinatorDeps::new` seeded, and leadership then waits on a scope no \
         provider action ever enters",
    );
    assert!(
        agent.contains("djinn_coordinator::CoordinatorHandle::spawn(deps.inner)"),
        "and the spawn helper must hand over that same `inner`, or the forwarded \
         scope never reaches the actor at all",
    );
}
