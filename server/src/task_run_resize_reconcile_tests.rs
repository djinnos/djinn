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
use std::collections::BTreeMap;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc as StdArc, Mutex as StdMutex};
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;

#[derive(Clone, Debug, Default)]
struct CapturedEvent {
    fields: BTreeMap<String, String>,
}

#[derive(Clone, Default)]
struct EventCapture(StdArc<StdMutex<Vec<CapturedEvent>>>);

impl EventCapture {
    fn events(&self) -> Vec<CapturedEvent> {
        self.0.lock().expect("captured events mutex").clone()
    }
}

#[derive(Default)]
struct EventFieldRecorder(CapturedEvent);

impl Visit for EventFieldRecorder {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.fields.insert(
            field.name().to_owned(),
            format!("{value:?}").trim_matches('"').to_owned(),
        );
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0
            .fields
            .insert(field.name().to_owned(), value.to_owned());
    }
}

impl<S: tracing::Subscriber> Layer<S> for EventCapture {
    fn on_event(&self, event: &tracing::Event<'_>, _context: Context<'_, S>) {
        let mut recorder = EventFieldRecorder::default();
        event.record(&mut recorder);
        self.0
            .lock()
            .expect("captured events mutex")
            .push(recorder.0);
    }
}

#[test]
fn lifecycle_and_lease_summaries_name_and_count_their_own_ledgers() {
    let captured = EventCapture::default();
    let subscriber = tracing_subscriber::registry().with(captured.clone());
    let pass = ResizeReconcilePass {
        mode: Some(ResizeReconcileMode::Enforce),
        scanned: 11,
        resumed: 7,
        would_resume: 4,
        permits_retired: 5,
        pre_birth_scanned: 3,
        pre_birth_reaped: 2,
        leases_released: 1,
        unsettled: 1,
        skipped: vec![("live-owner".to_owned(), SkipReason::OwnerLive)],
        scan_failed: true,
        pre_birth_scan_failed: true,
        ..ResizeReconcilePass::default()
    };

    tracing::subscriber::with_default(subscriber, || emit_pass_summaries(&pass));

    let events = captured.events();
    assert_eq!(events.len(), 2, "one structured summary per mutated ledger");
    let lifecycle = events
        .iter()
        .find(|event| event.fields.get("ledger") == Some(&"build_pod_permits".to_owned()))
        .expect("captured build_pod_permits lifecycle summary");
    assert_eq!(lifecycle.fields.get("scanned"), Some(&"11".to_owned()));
    assert_eq!(lifecycle.fields.get("resumed"), Some(&"7".to_owned()));
    assert_eq!(
        lifecycle.fields.get("would_resume"),
        Some(&"4".to_owned()),
        "observe-only work remains a lifecycle count"
    );
    assert_eq!(
        lifecycle.fields.get("permits_retired"),
        Some(&"5".to_owned()),
        "the lifecycle event reports the exact retired-permit count"
    );
    assert_eq!(
        lifecycle.fields.get("pre_birth_scanned"),
        Some(&"3".to_owned())
    );
    assert_eq!(
        lifecycle.fields.get("pre_birth_reaped"),
        Some(&"2".to_owned())
    );

    assert_eq!(
        lifecycle.fields.get("skipped"),
        Some(&"1".to_owned()),
        "the lifecycle event retains its own observe/skip population"
    );
    assert_eq!(
        lifecycle.fields.get("scan_failed"),
        Some(&"true".to_owned())
    );
    assert_eq!(
        lifecycle.fields.get("pre_birth_scan_failed"),
        Some(&"true".to_owned())
    );

    let leases = events
        .iter()
        .find(|event| event.fields.get("ledger") == Some(&"build_leases".to_owned()))
        .expect("captured build_leases release summary");
    assert_eq!(leases.fields.get("leases_released"), Some(&"1".to_owned()));
    assert!(
        !leases.fields.contains_key("permits_retired"),
        "a permit retirement is not lease reclamation"
    );
    assert!(
        !leases.fields.contains_key("scanned"),
        "lease reporting must not relabel permit scans as lease scans"
    );
    assert!(
        !leases.fields.contains_key("resumed")
            && !leases.fields.contains_key("pre_birth_reaped")
            && !leases.fields.contains_key("scan_failed"),
        "lease reporting must not carry lifecycle work or lifecycle failures"
    );
}

/// The drop-transit grace must stay ANCHORED to the handler budget it is
/// protecting, not to a number somebody typed once.
///
/// `DROP_TRANSIT_GRACE` exists to cover the whole window in which a LIVE
/// `release_lease` can still be mid-drop. That window is bounded by
/// `DROP_GATE_BUDGET`, which in turn contains `DROP_CONFIRMATION_BUDGET`. A
/// grace shorter than the gate budget lets the reconciler reach a row while the
/// handler that owns it is still inside its own wait — the defect, restored.
///
/// NAMED FAILING MUTATION: write `DROP_TRANSIT_GRACE` as a literal
/// `Duration::from_secs(90)`. The first assertion still passes, and then
/// changing `DROP_GATE_BUDGET` (which is what a future tuning pass would do)
/// silently breaks the relationship — which is what the second and third
/// assertions catch, because they are stated as inequalities against the
/// constants rather than against 90.
#[test]
fn the_drop_transit_grace_is_derived_from_the_budget_it_protects() {
    assert_eq!(
        DROP_TRANSIT_GRACE,
        DROP_GATE_BUDGET * 2,
        "the grace must be twice the handler's own wait budget, so a worker \
         that retries once after a `Held` verdict is still inside it"
    );
    assert!(
        DROP_TRANSIT_GRACE > DROP_GATE_BUDGET,
        "a grace no longer than the handler's wait budget lets the reconciler \
         resume a drop the handler is still performing"
    );
    assert!(
        DROP_TRANSIT_GRACE > crate::task_run_resize_drop::DROP_CONFIRMATION_BUDGET,
        "a grace shorter than the confirmation budget would strand a row while \
         the kubelet is still legitimately being waited on"
    );
    assert!(
        DROP_TRANSIT_GRACE < Duration::from_secs(600),
        "the grace is also an upper bound on how long a genuinely abandoned \
         drop keeps its build_leases row; it must stay small enough that \
         reconciliation is still the point of this module"
    );
}

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
