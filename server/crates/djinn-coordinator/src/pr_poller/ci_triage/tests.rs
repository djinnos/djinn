//! Regression tests for causal ranking of blocking CI checks.
//!
//! The centrepiece is [`tlu1_cascade_selects_the_lane_that_actually_ran`],
//! reconstructed field-for-field from GitHub Actions run `30087861197` on PR
//! #2525 — the incident that cost six sessions and three reopens.

use super::*;
use djinn_provider::github_api::{CheckAnnotation, CheckRun, CheckRunOutput};

/// Build a check run with the structural fields that drive ranking.
fn check(
    name: &str,
    conclusion: &str,
    started_at: &str,
    completed_at: &str,
    annotations_count: u64,
) -> CheckRun {
    CheckRun {
        id: 1,
        run_id: Some(30_087_861_197),
        name: name.to_string(),
        status: "completed".to_string(),
        conclusion: Some(conclusion.to_string()),
        html_url: format!("https://github.com/djinnos/djinn/actions/runs/30087861197/job/{name}"),
        started_at: Some(started_at.to_string()),
        completed_at: Some(completed_at.to_string()),
        output: Some(CheckRunOutput {
            title: None,
            summary: None,
            annotations_count: Some(annotations_count),
        }),
    }
}

/// The exact blocking set the board rendered for PR #2525: one lane that ran
/// for ~4.5 minutes and hard-failed carrying the runner-host annotation, five
/// cancelled siblings that all ran for minutes, a two-second `needs:`
/// aggregator, and a never-executed aggregator whose `completed_at` PRECEDES
/// its own `started_at`.
fn tlu1_cascade() -> Vec<CheckRun> {
    vec![
        // The real root cause. Runner host filled its own disk; the only trace
        // was an annotation on this job.
        check(
            "Plan Server Test Shards",
            "failure",
            "2026-07-24T10:57:45Z",
            "2026-07-24T11:02:24Z",
            1,
        ),
        // Swept up by the run-level `gh run cancel`. All genuinely ran.
        check(
            "Server Test (shard-1, 0)",
            "cancelled",
            "2026-07-24T10:57:45Z",
            "2026-07-24T11:02:55Z",
            0,
        ),
        check(
            "Server Test (shard-2, 1)",
            "cancelled",
            "2026-07-24T10:58:15Z",
            "2026-07-24T11:02:46Z",
            0,
        ),
        check(
            "Server Test (shard-3, 2)",
            "cancelled",
            "2026-07-24T10:57:45Z",
            "2026-07-24T11:02:48Z",
            0,
        ),
        check(
            "Server Test (shard-4, 3)",
            "cancelled",
            "2026-07-24T10:58:06Z",
            "2026-07-24T11:02:46Z",
            0,
        ),
        check(
            "Server sqlx Freshness",
            "cancelled",
            "2026-07-24T10:57:45Z",
            "2026-07-24T11:02:35Z",
            0,
        ),
        // `needs:`-dependent aggregator. Ran for two seconds evaluating
        // `needs.*.result`; produced no diagnosis of its own.
        check(
            "Quality Gate",
            "failure",
            "2026-07-24T11:02:58Z",
            "2026-07-24T11:03:00Z",
            0,
        ),
        // Never executed: completed_at is BEFORE started_at. This is the check
        // the old alphabetical derivation named as `primary_blocking_check`.
        check(
            "Publish Nextest Timing",
            "cancelled",
            "2026-07-24T11:02:56Z",
            "2026-07-24T11:02:55Z",
            0,
        ),
    ]
}

#[test]
fn tlu1_cascade_selects_the_lane_that_actually_ran() {
    let runs = tlu1_cascade();
    let blocking: Vec<&CheckRun> = runs.iter().collect();

    let primary = primary_blocking_check(&blocking).expect("cascade has one causal lane");
    assert_eq!(
        primary.name, "Plan Server Test Shards",
        "must select the lane that executed and hard-failed, not an aggregator"
    );
}

#[test]
fn tlu1_cascade_never_selects_the_never_executed_aggregator() {
    // The historical derivation was "the first element of the blocking list",
    // and that list was never sorted — it carried GitHub's check-runs API
    // order, which is arbitrary. On the real incident that first element was
    // the never-executed aggregator. Reproduce that ordering exactly.
    let mut runs = tlu1_cascade();
    let aggregator_at = runs
        .iter()
        .position(|c| c.name == "Publish Nextest Timing")
        .unwrap();
    let aggregator = runs.remove(aggregator_at);
    runs.insert(0, aggregator);

    let blocking: Vec<&CheckRun> = runs.iter().collect();
    assert_eq!(
        blocking[0].name, "Publish Nextest Timing",
        "guard the premise: unranked API order is what named the aggregator"
    );

    let primary = primary_blocking_check(&blocking).unwrap();
    assert_eq!(
        primary.name, "Plan Server Test Shards",
        "ranking must be independent of the order GitHub happened to return"
    );
    assert_ne!(primary.name, "Publish Nextest Timing");
    assert_ne!(primary.name, "Quality Gate");
}

#[test]
fn never_executed_aggregator_is_classified_as_such() {
    let runs = tlu1_cascade();
    let by_name = |n: &str| runs.iter().find(|c| c.name == n).unwrap();

    assert_eq!(
        check_evidence(by_name("Publish Nextest Timing")),
        CheckEvidence::NeverExecuted,
        "negative execution interval with no annotations means it never ran"
    );
    assert!(!executed(by_name("Publish Nextest Timing")));

    assert_eq!(
        check_evidence(by_name("Plan Server Test Shards")),
        CheckEvidence::Causal
    );
    assert_eq!(
        check_evidence(by_name("Server Test (shard-1, 0)")),
        CheckEvidence::RanThenCancelled,
        "a cancelled lane that genuinely ran is weak evidence, not causal"
    );
    assert_eq!(
        check_evidence(by_name("Quality Gate")),
        CheckEvidence::Causal,
        "a 2s aggregator did execute and did hard-fail; it is demoted by \
         earliest-start, not by pretending it never ran"
    );
}

#[test]
fn ranking_puts_causal_first_and_never_executed_last() {
    let runs = tlu1_cascade();
    let blocking: Vec<&CheckRun> = runs.iter().collect();
    let ranked = rank_blocking_checks(&blocking);
    let names: Vec<&str> = ranked.iter().map(|cr| cr.name.as_str()).collect();

    assert_eq!(names[0], "Plan Server Test Shards");
    assert_eq!(
        names[1], "Quality Gate",
        "the other causal lane follows, ordered by start time"
    );
    assert_eq!(
        *names.last().unwrap(),
        "Publish Nextest Timing",
        "the never-executed aggregator sorts last"
    );
}

#[test]
fn earliest_start_orders_dependents_after_their_dependencies() {
    // A `needs:` dependent cannot start before its dependency completes, so
    // ordering by started_at is topological without knowing the graph. Both
    // lanes here are causal; only start time separates them.
    let upstream = check(
        "zzz-upstream-lane",
        "failure",
        "2026-07-24T10:00:00Z",
        "2026-07-24T10:05:00Z",
        0,
    );
    let aggregator = check(
        "aaa-aggregator",
        "failure",
        "2026-07-24T10:05:01Z",
        "2026-07-24T10:05:03Z",
        0,
    );
    let blocking = vec![&aggregator, &upstream];

    assert_eq!(
        primary_blocking_check(&blocking).unwrap().name,
        "zzz-upstream-lane",
        "start order must beat name order"
    );
}

#[test]
fn annotations_break_ties_between_simultaneous_lanes() {
    let quiet = check(
        "aaa-quiet",
        "failure",
        "2026-07-24T10:00:00Z",
        "2026-07-24T10:05:00Z",
        0,
    );
    let diagnosing = check(
        "zzz-diagnosing",
        "failure",
        "2026-07-24T10:00:00Z",
        "2026-07-24T10:05:00Z",
        3,
    );
    let blocking = vec![&quiet, &diagnosing];
    assert_eq!(
        primary_blocking_check(&blocking).unwrap().name,
        "zzz-diagnosing"
    );
}

#[test]
fn all_cancelled_is_inconclusive_with_no_primary() {
    let runs: Vec<CheckRun> = vec![
        check(
            "Server Test (shard-1, 0)",
            "cancelled",
            "2026-07-24T10:57:45Z",
            "2026-07-24T11:02:55Z",
            0,
        ),
        check(
            "Publish Nextest Timing",
            "cancelled",
            "2026-07-24T11:02:56Z",
            "2026-07-24T11:02:55Z",
            0,
        ),
    ];
    let blocking: Vec<&CheckRun> = runs.iter().collect();

    assert!(is_inconclusive(&blocking));
    assert!(primary_blocking_check(&blocking).is_none());
    assert!(
        causal_checks(&blocking).is_empty(),
        "nothing causal means nothing may enter the failure fingerprint"
    );
}

#[test]
fn a_genuine_failure_makes_the_run_conclusive() {
    let runs = tlu1_cascade();
    let blocking: Vec<&CheckRun> = runs.iter().collect();
    assert!(!is_inconclusive(&blocking));
}

#[test]
fn empty_blocking_set_is_not_inconclusive() {
    assert!(!is_inconclusive(&[]));
}

#[test]
fn causal_checks_exclude_cancelled_and_never_executed() {
    let runs = tlu1_cascade();
    let blocking: Vec<&CheckRun> = runs.iter().collect();
    let names: Vec<&str> = causal_checks(&blocking)
        .iter()
        .map(|cr| cr.name.as_str())
        .collect();
    assert_eq!(names, vec!["Plan Server Test Shards", "Quality Gate"]);
}

#[test]
fn missing_timestamps_are_assumed_executed() {
    // GitHub omitting a field must never demote a real failure.
    let cr = CheckRun {
        id: 7,
        run_id: None,
        name: "Legacy Status Check".to_string(),
        status: "completed".to_string(),
        conclusion: Some("failure".to_string()),
        html_url: String::new(),
        started_at: None,
        completed_at: None,
        output: None,
    };
    assert!(executed(&cr));
    assert_eq!(check_evidence(&cr), CheckEvidence::Causal);
}

#[test]
fn annotations_prove_execution_even_with_a_degenerate_interval() {
    let cr = check(
        "Weird Timestamps",
        "failure",
        "2026-07-24T11:02:56Z",
        "2026-07-24T11:02:55Z",
        2,
    );
    assert!(executed(&cr), "annotations are proof of execution");
    assert_eq!(check_evidence(&cr), CheckEvidence::Causal);
}

// ── Annotation capture ───────────────────────────────────────────────────────

fn annotation(level: &str, message: &str) -> CheckAnnotation {
    CheckAnnotation {
        path: ".github".to_string(),
        start_line: 1,
        end_line: 1,
        annotation_level: level.to_string(),
        message: message.to_string(),
        title: None,
    }
}

#[test]
fn runner_host_failure_is_surfaced_verbatim() {
    let anns = vec![annotation(
        "failure",
        "System.IO.IOException: No space left on device : \
         '/home/runner/actions-runner/cached/2.336.0/_diag/Worker_20260724-105745-utc.log'\n \
         at GitHub.Runner.Worker.Worker.RunAsync(...)",
    )];
    let rendered = render_annotations("Plan Server Test Shards", &anns).unwrap();

    assert!(rendered.contains("Plan Server Test Shards"));
    assert!(
        rendered.contains("No space left on device"),
        "the whole point: the agent reads the cause without opening GitHub"
    );
    assert!(rendered.contains("[failure]"));
}

#[test]
fn empty_annotations_render_nothing() {
    assert!(render_annotations("Some Check", &[]).is_none());
}

#[test]
fn failure_annotations_are_ordered_before_warnings() {
    let anns = vec![
        annotation("warning", "unused variable"),
        annotation("failure", "No space left on device"),
    ];
    let rendered = render_annotations("Check", &anns).unwrap();
    let failure_at = rendered.find("No space left").unwrap();
    let warning_at = rendered.find("unused variable").unwrap();
    assert!(failure_at < warning_at);
}

#[test]
fn annotation_block_is_bounded_for_prompt_use() {
    let anns: Vec<CheckAnnotation> = (0..50)
        .map(|i| annotation("failure", &format!("annotation number {i} ").repeat(200)))
        .collect();
    let rendered = render_annotations("Noisy Check", &anns).unwrap();

    assert!(
        rendered.chars().count() <= MAX_ANNOTATION_BLOCK_CHARS + 32,
        "rendered {} chars exceeds the prompt bound",
        rendered.chars().count()
    );
    assert!(rendered.contains("truncated"));
}

#[test]
fn overflowing_annotation_count_is_reported_not_silently_dropped() {
    let anns: Vec<CheckAnnotation> = (0..MAX_CAPTURED_ANNOTATIONS + 3)
        .map(|i| annotation("failure", &format!("problem {i}")))
        .collect();
    let rendered = render_annotations("Check", &anns).unwrap();
    assert!(rendered.contains("+3 more annotation(s) not shown"));
}

#[test]
fn multibyte_annotations_do_not_panic_on_truncation() {
    let anns = vec![annotation("failure", &"é".repeat(5_000))];
    let rendered = render_annotations("Check", &anns).unwrap();
    assert!(rendered.chars().count() <= MAX_ANNOTATION_BLOCK_CHARS + 32);
}
