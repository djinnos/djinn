//! Shadow-epoch observation arms for `LeaseInvocationRunner`.
//!
//! Split out of `process_lease_tests.rs`, which was inside a kilobyte of the
//! 51200-byte size guard. The scenario is self-contained: it drives the runner
//! end to end under a shadow epoch and asserts the two bounded counter arms.

use super::*;

/// Both shadow-observation arms come from real invocation paths, so the shadow
/// window produces a ratio rather than a single half of one.
///
/// `would_escalate` is produced by an invocation that crossed the escalation
/// threshold and reached a valid matching bind under a shadow epoch;
/// `would_throttle` by one that ran to terminal below the threshold and was
/// never escalated. Neither counter is incremented by the test: each is emitted
/// by driving `LeaseInvocationRunner` end to end. The whole scenario shares one
/// current-thread runtime because the isolated recorder is thread-local.
///
/// The `class` label is published by the process, never threaded through the
/// runner: the runner takes no role input, and this test is the proof that the
/// label rides along without becoming one. It is published on this thread so
/// the assertion is deterministic under the same thread-local scoping
/// `render_isolated` uses.
#[test]
fn shadow_epoch_emits_both_would_throttle_arms_from_production_paths() {
    let (_, rendered) = djinn_telemetry::render_isolated(|| {
        djinn_telemetry::role_class::observe(djinn_telemetry::role_class::CLASS_BUILD_CAPABLE);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime builds");
        runtime.block_on(async {
            // ── Escalating arm: shadow epoch + a valid matching bind. ──────
            let services = Arc::new(ScriptedServices::new(
                vec![granted(7)],
                vec![status(LeaseState::Active, Some(7))],
                vec![status(LeaseState::Active, Some(7)); 20],
            ));
            services.set_lift_decision(djinn_supervisor::services::InvocationLiftDecision::Shadow);
            services
                .release
                .lock()
                .unwrap()
                .push_back(LeaseResult::Released {
                    candidate_cleanup: false,
                });
            let launcher = Arc::new(ScriptedLauncher::default());
            let cancel = CancellationToken::new();
            let runner = LeaseInvocationRunner::new(services.clone(), launcher.clone(), clock());
            let run_cancel = cancel.clone();
            let run =
                tokio::spawn(async move { runner.output(command(), config(), run_cancel).await });
            wait_for(&services.status_calls, 3).await;
            cancel.cancel();
            run.await.unwrap().unwrap();
            assert_eq!(services.grant_calls.load(Ordering::SeqCst), 1);
            assert!(
                launcher.lifts.lock().unwrap().is_empty(),
                "the escalating shadow arm must still never lift cpu.max"
            );

            // ── Non-escalating arm: the child never crosses the escalation
            // threshold, so the lease authority is never contacted at all. ──
            let services = Arc::new(ScriptedServices::new(vec![], vec![], vec![]));
            services.set_lift_decision(djinn_supervisor::services::InvocationLiftDecision::Shadow);
            let launcher = Arc::new(BrokerBackedLauncher::running(0));
            let runner = LeaseInvocationRunner::new(services.clone(), launcher.clone(), clock());
            let cancel = CancellationToken::new();
            let run_cancel = cancel.clone();
            let run =
                tokio::spawn(async move { runner.output(command(), config(), run_cancel).await });
            for _ in 0..10_000 {
                if launcher.state.lock().unwrap().samples > 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert!(
                launcher.state.lock().unwrap().samples > 0,
                "the below-threshold child was never sampled"
            );
            cancel.cancel();
            run.await.unwrap().unwrap();
            assert_eq!(
                services.queue_calls.load(Ordering::SeqCst),
                0,
                "a below-threshold invocation never escalates"
            );
            assert_eq!(services.grant_calls.load(Ordering::SeqCst), 0);
        });
    });

    let samples: Vec<&str> = rendered
        .lines()
        .filter(|line| line.starts_with("djinn_build_admission_shadow_invocation_total{"))
        .collect();
    assert_eq!(
        samples.len(),
        2,
        "both shadow arms emit exactly one bounded series each:\n{rendered}"
    );
    for (decision, sample) in [("would_escalate", 1), ("would_throttle", 1)]
        .into_iter()
        .map(|(decision, value)| {
            (
                decision,
                format!(
                    "djinn_build_admission_shadow_invocation_total{{decision=\"{decision}\",class=\"build-capable\"}} {value}"
                ),
            )
        })
    {
        assert!(
            rendered.lines().any(|line| line == sample),
            "missing the {decision} arm in:\n{rendered}"
        );
    }
    // Bounded cardinality: `decision` and `class` are the only labels, and both
    // domains are closed (2 x 2 series, forever).
    for sample in samples {
        let labels = sample
            .split_once('{')
            .and_then(|(_, rest)| rest.split_once('}'))
            .map(|(labels, _)| labels)
            .expect("rendered sample carries a label block");
        let keys: Vec<&str> = labels
            .split(',')
            .map(|label| label.split_once('=').expect("label key/value").0)
            .collect();
        assert_eq!(
            keys,
            ["decision", "class"],
            "shadow invocation samples carry only the two bounded labels: {labels}"
        );
        assert!(
            djinn_telemetry::role_class::ALL_CLASSES
                .iter()
                .any(|class| labels.contains(&format!("class=\"{class}\""))),
            "class label must stay inside the two-value vocabulary: {labels}"
        );
    }
}
