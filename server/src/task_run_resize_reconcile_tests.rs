//! Unit tests for [`super`]'s pure seams: the per-tick mode and the ticker.
//!
//! Everything that is a statement about DURABLE resumption lives in
//! `server/tests/task_run_resize_recovery.rs`, against real Postgres. Nothing
//! here stands in for that: a mode enum and a ticker are the only parts of this
//! module that have no database in them, and they are tested here precisely so
//! the durable file is not diluted with assertions that would pass against a
//! fake.
//!
//! The question every test answers: "what stays green if the body of this does
//! nothing?"

use super::*;
use std::sync::atomic::AtomicUsize;

/// An unrecognised configuration value must ARM the reconciler, not disarm it.
///
/// A typo in a deployment value deciding that stranded Pods keep their CPU is
/// the wrong direction to fail in, and a default that silently reads `off` is
/// how this repository has repeatedly shipped an inert subsystem.
///
/// NAMED FAILING MUTATION: make the fallback arm `Self::Off` and the last two
/// rows fail.
#[test]
fn an_unrecognised_mode_arms_rather_than_disarms() {
    for raw in ["off", "OFF", " off ", "0", "false", "disabled"] {
        assert_eq!(
            ResizeReconcileMode::parse(raw),
            ResizeReconcileMode::Off,
            "{raw} must disarm"
        );
    }
    for raw in ["observe", "dry_run", "dry-run"] {
        assert_eq!(
            ResizeReconcileMode::parse(raw),
            ResizeReconcileMode::Observe
        );
    }
    for raw in ["enforce", "on", "1", "", "yes-please", "enfroce"] {
        assert_eq!(
            ResizeReconcileMode::parse(raw),
            ResizeReconcileMode::Enforce,
            "{raw} must NOT silently disarm the reconciler"
        );
    }
    assert_eq!(ResizeReconcileMode::default(), ResizeReconcileMode::Enforce);
    assert!(!ResizeReconcileMode::Off.acts());
    assert!(!ResizeReconcileMode::Observe.acts());
    assert!(ResizeReconcileMode::Enforce.acts());
}

/// The FIRST tick fires before any interval elapses, and every later tick keeps
/// firing until cancellation.
///
/// The startup scan and the periodic sweep are the same code path here, which
/// is the structural reason this module cannot repeat the startup-only-reaper
/// failure: there is no separate one-shot to forget to make periodic.
///
/// NAMED FAILING MUTATION: add a leading `ticker.tick().await;` before the loop
/// (which is what `graph_retention::run_loop` does to SKIP the immediate tick)
/// and the first assertion fails — no tick lands inside the paused clock's
/// first interval.
#[tokio::test(start_paused = true)]
async fn the_first_tick_is_immediate_and_the_loop_keeps_ticking() {
    let ticks = Arc::new(AtomicUsize::new(0));
    let cancel = CancellationToken::new();
    let loop_ticks = Arc::clone(&ticks);
    let loop_cancel = cancel.clone();
    let handle = tokio::spawn(async move {
        run_loop(Duration::from_secs(30), loop_cancel, move || {
            let ticks = Arc::clone(&loop_ticks);
            async move {
                ticks.fetch_add(1, Ordering::SeqCst);
            }
        })
        .await;
    });

    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(
        ticks.load(Ordering::SeqCst),
        1,
        "the startup scan must not wait a full interval: a server that restarts \
         with stranded rows has to see them now"
    );

    tokio::time::sleep(Duration::from_secs(95)).await;
    let periodic = ticks.load(Ordering::SeqCst);
    assert!(
        periodic >= 4,
        "the sweep must be PERIODIC, not startup-only; saw {periodic} ticks in \
         95s at a 30s cadence"
    );

    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("the loop must stop on cancellation")
        .expect("the loop task must not panic");
    let at_cancel = ticks.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(120)).await;
    assert_eq!(
        ticks.load(Ordering::SeqCst),
        at_cancel,
        "a cancelled loop must stop ticking"
    );
}

/// The production gate re-reads its environment on EVERY call.
///
/// This is the half of acceptance criterion 2 that is about the gate itself;
/// that the LOOP calls it once per tick is proven durably in
/// `server/tests/task_run_resize_recovery.rs`, where a stranded row is left
/// alone and then picked up without a restart.
///
/// NAMED FAILING MUTATION: cache the parsed mode in `EnvResizeReconcileGate`
/// (a field set at construction) and the second half of this test fails,
/// because the gate would still report the value it was built with.
#[test]
fn the_env_gate_rereads_its_variable_on_every_call() {
    // SAFETY: single-threaded `#[test]`, and the variable is restored before
    // returning. It is namespaced to this module and no other test reads it.
    let restore = std::env::var(MODE_ENV).ok();
    let gate = EnvResizeReconcileGate;

    unsafe { std::env::set_var(MODE_ENV, "off") };
    assert_eq!(gate.mode(), ResizeReconcileMode::Off);

    unsafe { std::env::set_var(MODE_ENV, "enforce") };
    assert_eq!(
        gate.mode(),
        ResizeReconcileMode::Enforce,
        "the SAME gate object must observe the change: arming is a configuration \
         change, not a restart"
    );

    unsafe { std::env::remove_var(MODE_ENV) };
    assert_eq!(
        gate.mode(),
        ResizeReconcileMode::Enforce,
        "unset must arm; an operator who never sets this must still be protected \
         from stranded Pods"
    );

    match restore {
        Some(value) => unsafe { std::env::set_var(MODE_ENV, value) },
        None => unsafe { std::env::remove_var(MODE_ENV) },
    }
}

/// A configured cadence overrides the default, and a nonsense one does not.
///
/// NAMED FAILING MUTATION: drop the `> 0` filter and `0` yields a zero-duration
/// interval, which `tokio::time::interval` panics on — a spin loop against
/// Postgres in production.
#[test]
fn the_sweep_interval_rejects_values_that_would_spin() {
    let restore = std::env::var(INTERVAL_ENV).ok();

    unsafe { std::env::remove_var(INTERVAL_ENV) };
    assert_eq!(sweep_interval_from_env(), DEFAULT_SWEEP_INTERVAL);

    unsafe { std::env::set_var(INTERVAL_ENV, "5") };
    assert_eq!(sweep_interval_from_env(), Duration::from_secs(5));

    for bad in ["0", "-1", "soon", ""] {
        unsafe { std::env::set_var(INTERVAL_ENV, bad) };
        assert_eq!(
            sweep_interval_from_env(),
            DEFAULT_SWEEP_INTERVAL,
            "{bad} must fall back rather than produce a spinning ticker"
        );
    }

    match restore {
        Some(value) => unsafe { std::env::set_var(INTERVAL_ENV, value) },
        None => unsafe { std::env::remove_var(INTERVAL_ENV) },
    }
}
