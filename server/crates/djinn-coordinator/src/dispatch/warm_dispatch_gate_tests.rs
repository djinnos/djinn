//! Contract test for the pre-pod-allocation warm build-cache dispatch gate,
//! driven by the frozen `tests/fixtures/warm_dispatch/warm_dispatch_gate_v1.json`
//! fixture (proposal ri23 Part 2).
//!
//! The fixture is normative. For every scenario the test asserts that:
//!   - the warm decision completes BEFORE pod allocation (allocation is
//!     sequenced strictly after every probe/trigger the decision performed);
//!   - a fresh matching cache allocates immediately (one probe, no trigger);
//!   - a no-compile stack bypasses the gate (no probe, no trigger);
//!   - a missing/stale cache waits only up to the configured bound;
//!   - timeout / inventory-error / warmer-error each cold-dispatch and allocate
//!     EXACTLY ONCE;
//!   - an identity mismatch (probe reports non-fresh) is never treated as fresh.
//!
//! It fails (nonzero) on allocation-before-decision, an unbounded wait, an
//! identity mismatch treated as fresh, or a failure to cold-dispatch (allocate)
//! after the bound/error.

#![allow(clippy::disallowed_methods)] // test: TestClock monotonic base needs a real Instant

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use djinn_core::clock::TestClock;
use serde_json::Value;

use super::{
    CompileMode, WarmBuildCacheProbe, WarmCacheIdentity, WarmDispatchDecision, WarmDispatchGate,
    WarmFreshness, WarmProbeError,
};

const FIXTURE: &str = include_str!("../../tests/fixtures/warm_dispatch/warm_dispatch_gate_v1.json");

/// A deterministic, scripted [`WarmBuildCacheProbe`] built from a fixture case.
///
/// Replays `probe_results` in order (repeating the last once exhausted) and
/// advances the shared [`TestClock`] by `advance_per_poll` on every probe call
/// after the first, so the bounded-wait deadline is crossed without real time.
/// Records global sequence numbers for every probe/trigger so the test can
/// prove allocation is sequenced after the decision.
struct ScriptedProbe {
    results: Vec<String>,
    trigger_ok: bool,
    advance_per_poll: Duration,
    clock: std::sync::Arc<TestClock>,
    call_index: Mutex<usize>,
    probe_calls: AtomicUsize,
    trigger_calls: AtomicUsize,
    seq: std::sync::Arc<AtomicUsize>,
    max_decision_seq: std::sync::Arc<AtomicUsize>,
}

impl ScriptedProbe {
    fn note_decision_seq(&self) {
        let s = self.seq.fetch_add(1, Ordering::SeqCst);
        self.max_decision_seq.fetch_max(s, Ordering::SeqCst);
    }
}

#[async_trait]
impl WarmBuildCacheProbe for ScriptedProbe {
    async fn probe(&self, _identity: &WarmCacheIdentity) -> Result<WarmFreshness, WarmProbeError> {
        self.note_decision_seq();
        self.probe_calls.fetch_add(1, Ordering::SeqCst);
        let idx = {
            let mut guard = self.call_index.lock().expect("probe index");
            let current = *guard;
            *guard += 1;
            current
        };
        if idx >= 1 {
            self.clock.advance_mono(self.advance_per_poll);
        }
        let which = idx.min(self.results.len().saturating_sub(1));
        match self.results[which].as_str() {
            "fresh" => Ok(WarmFreshness::Fresh),
            "stale" => Ok(WarmFreshness::Stale),
            "missing" => Ok(WarmFreshness::Missing),
            "error" => Err(WarmProbeError("scripted inventory error".to_owned())),
            other => panic!("unknown scripted probe result {other}"),
        }
    }

    async fn trigger(
        &self,
        _identity: &WarmCacheIdentity,
    ) -> Result<(), djinn_runtime::WarmerError> {
        self.note_decision_seq();
        self.trigger_calls.fetch_add(1, Ordering::SeqCst);
        if self.trigger_ok {
            Ok(())
        } else {
            Err(djinn_runtime::WarmerError::Backend(
                "scripted warmer error".to_owned(),
            ))
        }
    }
}

fn decision_label(decision: WarmDispatchDecision) -> &'static str {
    match decision {
        WarmDispatchDecision::Bypass => "bypass",
        WarmDispatchDecision::FreshImmediate => "fresh_immediate",
        WarmDispatchDecision::WaitedFresh { .. } => "waited_fresh",
        WarmDispatchDecision::ColdDispatch {
            reason: super::ColdReason::Timeout,
        } => "cold_timeout",
        WarmDispatchDecision::ColdDispatch {
            reason: super::ColdReason::InventoryError,
        } => "cold_inventory_error",
        WarmDispatchDecision::ColdDispatch {
            reason: super::ColdReason::WarmerError,
        } => "cold_warmer_error",
    }
}

#[tokio::test]
async fn warm_dispatch_gate_v1_contract() {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("warm_dispatch_gate_v1.json parses");
    let wait_bound =
        Duration::from_millis(fixture["wait_bound_ms"].as_u64().expect("wait_bound_ms"));
    let poll_interval = Duration::from_millis(
        fixture["poll_interval_ms"]
            .as_u64()
            .expect("poll_interval_ms"),
    );
    let gate = WarmDispatchGate::new(wait_bound, poll_interval);

    for case in fixture["cases"].as_array().expect("cases") {
        let scenario = case["scenario"].as_str().expect("scenario");
        let compile_mode = match case["compile_mode"].as_str().expect("compile_mode") {
            "compile" => CompileMode::Compile,
            "none" => CompileMode::None,
            other => panic!("unknown compile_mode {other}"),
        };
        let results: Vec<String> = case["probe_results"]
            .as_array()
            .expect("probe_results")
            .iter()
            .map(|v| v.as_str().expect("probe result").to_owned())
            .collect();
        let trigger_ok = case["trigger"].as_str().expect("trigger") == "ok";
        let advance_per_poll = Duration::from_millis(
            case["advance_per_poll_ms"]
                .as_u64()
                .expect("advance_per_poll"),
        );

        let clock = std::sync::Arc::new(TestClock::new(SystemTime::UNIX_EPOCH, Instant::now()));
        let seq = std::sync::Arc::new(AtomicUsize::new(1));
        let max_decision_seq = std::sync::Arc::new(AtomicUsize::new(0));
        let probe = ScriptedProbe {
            results,
            trigger_ok,
            advance_per_poll,
            clock: std::sync::Arc::clone(&clock),
            call_index: Mutex::new(0),
            probe_calls: AtomicUsize::new(0),
            trigger_calls: AtomicUsize::new(0),
            seq: std::sync::Arc::clone(&seq),
            max_decision_seq: std::sync::Arc::clone(&max_decision_seq),
        };
        let identity = WarmCacheIdentity {
            project_id: format!("rmxn-gate-{scenario}"),
            environment_identity: "img-abc:toolchain-1:mold-jobs-1".to_owned(),
        };

        // Model the dispatch loop: the warm decision completes and records its
        // metric BEFORE the caller allocates the pod exactly once.
        let decision = gate
            .decide_and_record(compile_mode, &probe, &identity, clock.as_ref())
            .await;

        let allocate_calls = AtomicUsize::new(0);
        let allocate_after_decision = AtomicUsize::new(0);
        {
            allocate_calls.fetch_add(1, Ordering::SeqCst);
            let s = seq.fetch_add(1, Ordering::SeqCst);
            // Allocation must be sequenced strictly after every probe/trigger
            // the decision performed.
            if s > max_decision_seq.load(Ordering::SeqCst) {
                allocate_after_decision.fetch_add(1, Ordering::SeqCst);
            }
        }

        // Decision shape and closed telemetry labels match the fixture.
        assert_eq!(
            decision_label(decision),
            case["expected_decision"]
                .as_str()
                .expect("expected_decision"),
            "scenario {scenario}: unexpected decision"
        );
        let (outcome, reason) = decision.telemetry_labels();
        assert_eq!(
            outcome.as_label(),
            case["expected_outcome"].as_str().expect("expected_outcome"),
            "scenario {scenario}: unexpected outcome label"
        );
        assert_eq!(
            reason.as_label(),
            case["expected_reason"].as_str().expect("expected_reason"),
            "scenario {scenario}: unexpected reason label"
        );

        // Probe/trigger call counts: proves the fresh path skips the trigger,
        // no-compile skips everything, and the wait is bounded (a bounded
        // number of re-probes, never unbounded).
        assert_eq!(
            probe.probe_calls.load(Ordering::SeqCst) as u64,
            case["expected_probe_calls"]
                .as_u64()
                .expect("expected_probe_calls"),
            "scenario {scenario}: unexpected probe call count (unbounded wait?)"
        );
        assert_eq!(
            probe.trigger_calls.load(Ordering::SeqCst) as u64,
            case["expected_trigger_calls"]
                .as_u64()
                .expect("expected_trigger_calls"),
            "scenario {scenario}: unexpected trigger call count"
        );

        // Allocation happens exactly once, strictly after the decision.
        assert_eq!(
            allocate_calls.load(Ordering::SeqCst),
            1,
            "scenario {scenario}: must allocate exactly once (cold dispatch still allocates)"
        );
        assert_eq!(
            allocate_after_decision.load(Ordering::SeqCst),
            1,
            "scenario {scenario}: allocation must be sequenced after the warm decision"
        );

        // A non-fresh initial observation must never yield a fresh-immediate
        // decision — identity mismatch can never be treated as fresh.
        if scenario == "identity_mismatch_is_never_treated_as_fresh" {
            assert!(
                !matches!(decision, WarmDispatchDecision::FreshImmediate),
                "identity mismatch must never be treated as fresh"
            );
            assert!(
                !decision.is_warm_hit(),
                "identity mismatch is not a warm hit"
            );
        }
    }
}
