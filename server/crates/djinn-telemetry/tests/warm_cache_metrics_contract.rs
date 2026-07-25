//! Contract test for the warm build-cache dispatch-gate telemetry surface
//! (proposal ri23 Part 2), driven by the frozen
//! `fixtures/warm_cache/warm_cache_metrics_v1.json` fixture.
//!
//! The fixture is normative: it pins the metric name, the closed
//! `outcome`/`reason` label spaces, the per-scenario outcome/reason mapping,
//! and the cold-rate alert fire/recovery behaviour. A test here fails (nonzero)
//! for a missing/extra/unbounded label, a counter that stays dark, a duplicate
//! emission, a no-compile decision counted as cold, or an alert that fails to
//! fire or clear.

use djinn_telemetry::render_isolated;
use djinn_telemetry::warm_cache::{
    self, AlertTransition, ColdRateAlert, ColdRateAlertConfig, Outcome, Reason,
};
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/warm_cache/warm_cache_metrics_v1.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("warm_cache_metrics_v1.json parses")
}

fn outcome_from_label(label: &str) -> Outcome {
    warm_cache::ALL_OUTCOMES
        .into_iter()
        .find(|o| o.as_label() == label)
        .unwrap_or_else(|| panic!("unknown outcome label {label}"))
}

fn reason_from_label(label: &str) -> Reason {
    warm_cache::ALL_REASONS
        .into_iter()
        .find(|r| r.as_label() == label)
        .unwrap_or_else(|| panic!("unknown reason label {label}"))
}

/// The enum label spaces must equal the fixture's enumerations exactly. A
/// missing or extra variant on either side fails, keeping the series bounded.
#[test]
fn outcome_and_reason_label_spaces_match_the_fixture_exactly() {
    let fx = fixture();

    let fixture_outcomes: Vec<String> = fx["outcomes"]
        .as_array()
        .expect("outcomes array")
        .iter()
        .map(|v| v.as_str().expect("outcome string").to_owned())
        .collect();
    let enum_outcomes: Vec<String> = warm_cache::ALL_OUTCOMES
        .iter()
        .map(|o| o.as_label().to_owned())
        .collect();
    assert_eq!(
        enum_outcomes, fixture_outcomes,
        "Outcome label space must match the fixture exactly (no missing/extra labels)"
    );

    let fixture_reasons: Vec<String> = fx["reasons"]
        .as_array()
        .expect("reasons array")
        .iter()
        .map(|v| v.as_str().expect("reason string").to_owned())
        .collect();
    let enum_reasons: Vec<String> = warm_cache::ALL_REASONS
        .iter()
        .map(|r| r.as_label().to_owned())
        .collect();
    assert_eq!(
        enum_reasons, fixture_reasons,
        "Reason label space must match the fixture exactly (no missing/extra labels)"
    );
}

/// Each scenario records exactly one `(project_id, outcome, reason)` series with
/// value 1, exactly the three expected label keys, and no other series for the
/// project. This catches dark counters, duplicate emission, and stray labels.
#[test]
fn each_scenario_emits_exactly_one_bounded_decision_series() {
    let fx = fixture();
    let metric = fx["decision_metric"].as_str().expect("decision_metric");
    let cases = fx["decision_cases"].as_array().expect("decision_cases");

    for case in cases {
        let scenario = case["scenario"].as_str().expect("scenario");
        let outcome = outcome_from_label(case["outcome"].as_str().expect("outcome"));
        let reason = reason_from_label(case["reason"].as_str().expect("reason"));
        let project_id = format!("warm-cache-contract-{scenario}");

        let ((), rendered) =
            render_isolated(|| warm_cache::record_decision(&project_id, outcome, reason));

        let series: Vec<&str> = rendered
            .lines()
            .filter(|line| line.starts_with(metric) && line.contains(&project_id))
            .collect();
        assert_eq!(
            series.len(),
            1,
            "scenario {scenario} must emit exactly one decision series, got: {series:?}"
        );
        let line = series[0];
        assert!(
            line.contains(&format!("outcome=\"{}\"", outcome.as_label()))
                && line.contains(&format!("reason=\"{}\"", reason.as_label())),
            "scenario {scenario} series missing expected outcome/reason: {line}"
        );
        assert!(
            line.trim_end().ends_with(" 1"),
            "scenario {scenario} must increment exactly once (no duplicate emission): {line}"
        );

        // Exactly the three expected label keys — no extra/unbounded labels.
        let label_block = line
            .split_once('{')
            .and_then(|(_, rest)| rest.split_once('}'))
            .map(|(labels, _)| labels)
            .unwrap_or("");
        let keys: Vec<&str> = label_block
            .split(',')
            .filter_map(|kv| kv.split_once('=').map(|(k, _)| k))
            .collect();
        assert_eq!(
            keys,
            vec!["project_id", "outcome", "reason"],
            "scenario {scenario} must carry exactly the bounded label keys: {line}"
        );
    }
}

/// A no-compile bypass maps to `fallback`, never `cold`, at both the label layer
/// and the alert layer: any number of fallback decisions must never fire the
/// cold-rate alert.
#[test]
fn no_compile_is_fallback_and_never_drives_the_alert() {
    let fx = fixture();
    let no_compile = fx["decision_cases"]
        .as_array()
        .expect("decision_cases")
        .iter()
        .find(|c| c["scenario"].as_str() == Some("no_compile"))
        .expect("no_compile case present");
    assert_eq!(no_compile["outcome"].as_str(), Some("fallback"));
    assert_ne!(
        no_compile["outcome"].as_str(),
        Some("cold"),
        "no-compile must never be counted cold"
    );

    let mut alert = ColdRateAlert::new(ColdRateAlertConfig {
        fire_at: 0.5,
        clear_at: 0.2,
        min_samples: 5,
    });
    for _ in 0..100 {
        alert.observe(Outcome::Fallback);
        alert.evaluate();
    }
    assert!(
        !alert.firing(),
        "fallback (no-compile) decisions must never drive the cold-rate alert"
    );
    assert_eq!(alert.cold_rate(), 0.0);
}

/// The cold-rate alert must stay clear below the sample floor, fire once the
/// cold rate crosses `fire_at`, and clear again once it falls to `clear_at`.
#[test]
fn cold_rate_alert_fires_and_recovers_per_fixture() {
    let fx = fixture();
    let alert_fx = &fx["alert"];
    let cfg = &alert_fx["config"];
    let config = ColdRateAlertConfig {
        fire_at: cfg["fire_at"].as_f64().expect("fire_at"),
        clear_at: cfg["clear_at"].as_f64().expect("clear_at"),
        min_samples: cfg["min_samples"].as_u64().expect("min_samples"),
    };
    let mut alert = ColdRateAlert::new(config);

    // Below the sample floor: a 100% cold rate must NOT fire.
    let below = &alert_fx["below_min_samples"];
    for _ in 0..below["cold"].as_u64().expect("below cold") {
        alert.observe(Outcome::Cold);
    }
    assert_eq!(alert.evaluate(), AlertTransition::Unchanged);
    assert!(
        !alert.firing(),
        "alert must not fire below the min_samples floor"
    );

    // Cross the fire threshold.
    let fire = &alert_fx["fire"];
    for _ in 0..fire["cold"].as_u64().expect("fire cold") {
        alert.observe(Outcome::Cold);
    }
    for _ in 0..fire["hit"].as_u64().expect("fire hit") {
        alert.observe(Outcome::Hit);
    }
    let transition = alert.evaluate();
    assert_eq!(
        transition,
        AlertTransition::Fired,
        "alert must fire once the cold rate crosses fire_at"
    );
    assert!(alert.firing());

    // Drive the rate back down until it clears.
    let recover = &alert_fx["recover"];
    for _ in 0..recover["hit"].as_u64().expect("recover hit") {
        alert.observe(Outcome::Hit);
    }
    let transition = alert.evaluate();
    assert_eq!(
        transition,
        AlertTransition::Cleared,
        "alert must clear once the cold rate falls to clear_at"
    );
    assert!(!alert.firing());
}

/// The alert emits both gauges on every evaluation, so a firing state is always
/// observable in the Prometheus surface (fire and clear).
#[test]
fn cold_rate_alert_emits_firing_gauge_on_fire_and_clear() {
    let (_, rendered_fire) = render_isolated(|| {
        let mut alert = ColdRateAlert::new(ColdRateAlertConfig {
            fire_at: 0.5,
            clear_at: 0.2,
            min_samples: 2,
        });
        alert.observe(Outcome::Cold);
        alert.observe(Outcome::Cold);
        alert.evaluate();
    });
    assert!(
        rendered_fire
            .lines()
            .any(|l| l.starts_with(warm_cache::COLD_RATE_ALERT_FIRING)
                && l.trim_end().ends_with(" 1")),
        "firing gauge must be 1 while the alert fires:\n{rendered_fire}"
    );

    let (_, rendered_clear) = render_isolated(|| {
        let mut alert = ColdRateAlert::new(ColdRateAlertConfig {
            fire_at: 0.5,
            clear_at: 0.2,
            min_samples: 2,
        });
        alert.observe(Outcome::Hit);
        alert.observe(Outcome::Hit);
        alert.evaluate();
    });
    assert!(
        rendered_clear
            .lines()
            .any(|l| l.starts_with(warm_cache::COLD_RATE_ALERT_FIRING)
                && l.trim_end().ends_with(" 0")),
        "firing gauge must be 0 while the alert is clear:\n{rendered_clear}"
    );
}
