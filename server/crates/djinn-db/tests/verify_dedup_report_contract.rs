//! Deterministic, fixture-owned contract for the verification-dedup measurement.
//!
//! This is deliberately a pure report model: it consumes committed audit-shaped
//! events instead of a database, clock, credentials, or an operator's live
//! seven-day cohort. `query.sql` is the versioned production-query equivalent;
//! the model below makes its grouping and fail-closed rules executable in CI.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

const METADATA: &str = include_str!("fixtures/verify_dedup_report_v1/metadata.json");
const EVENTS: &str = include_str!("fixtures/verify_dedup_report_v1/events.json");
const QUERY: &str = include_str!("fixtures/verify_dedup_report_v1/query.sql");

#[derive(Debug, Deserialize)]
struct Metadata {
    query_version: String,
    ratio_must_be_less_than: f64,
    minimum_completed_task_runs_per_cohort: usize,
    minimum_distinct_fingerprints_per_cohort: usize,
    cohorts: Vec<Cohort>,
    declared_infrastructure_wide_outages: Vec<OutageInterval>,
}

#[derive(Debug, Deserialize)]
struct Cohort {
    name: String,
    start_inclusive: String,
    end_exclusive: String,
}

#[derive(Debug, Deserialize)]
struct OutageInterval {
    start_inclusive: String,
    end_exclusive: String,
}

#[derive(Debug, Deserialize)]
struct Fixture {
    events: Vec<Event>,
    boundary_cases: BTreeMap<String, BoundaryCase>,
}

#[derive(Clone, Debug, Deserialize)]
struct Event {
    id: String,
    timestamp: String,
    project_id: String,
    task_id: String,
    verification_input_fingerprint: Option<String>,
    event_kind: String,
    completed_task_run: bool,
    executes_build_command: bool,
    project_ci: bool,
    merge_queue_ci: bool,
    cancelled_before_first_command: bool,
    infrastructure_wide_outage: bool,
    submitted_c2_fingerprint: Option<String>,
    stored_fingerprint: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BoundaryCase {
    canonical_build_executions: usize,
    distinct_fingerprints: usize,
}

#[derive(Debug)]
struct CohortReport {
    completed_task_runs: usize,
    distinct_fingerprints: usize,
}

#[derive(Debug)]
struct Report {
    query_version: String,
    canonical_build_executions: usize,
    distinct_fingerprints: usize,
    ratio: f64,
    cohorts: BTreeMap<String, CohortReport>,
    exclusions: BTreeMap<&'static str, usize>,
}

fn is_declared_outage(metadata: &Metadata, timestamp: &str) -> bool {
    metadata
        .declared_infrastructure_wide_outages
        .iter()
        .any(|outage| {
            timestamp >= outage.start_inclusive.as_str()
                && timestamp < outage.end_exclusive.as_str()
        })
}

fn exclusion(metadata: &Metadata, event: &Event) -> Option<&'static str> {
    if event.project_ci {
        Some("project_ci")
    } else if event.merge_queue_ci {
        Some("merge_queue_ci")
    } else if event.cancelled_before_first_command {
        Some("cancelled_before_first_command")
    } else if is_declared_outage(metadata, &event.timestamp) {
        Some("infrastructure_wide_outage")
    } else if event.verification_input_fingerprint.is_none() {
        Some("missing_fingerprint")
    } else {
        None
    }
}

fn cohort_for<'a>(metadata: &'a Metadata, timestamp: &str) -> Option<&'a Cohort> {
    metadata.cohorts.iter().find(|cohort| {
        timestamp >= cohort.start_inclusive.as_str() && timestamp < cohort.end_exclusive.as_str()
    })
}

fn validate_ratio(
    canonical_build_executions: usize,
    distinct_fingerprints: usize,
    ratio_must_be_less_than: f64,
) -> Result<f64, String> {
    if distinct_fingerprints == 0 {
        return Err("no eligible fingerprints".into());
    }
    let ratio = canonical_build_executions as f64 / distinct_fingerprints as f64;
    if ratio >= ratio_must_be_less_than {
        return Err(format!(
            "ratio {ratio} must be below {ratio_must_be_less_than}"
        ));
    }
    Ok(ratio)
}

fn calculate_report(metadata: &Metadata, events: &[Event]) -> Result<Report, String> {
    let mut exclusions = BTreeMap::from([
        ("project_ci", 0),
        ("merge_queue_ci", 0),
        ("cancelled_before_first_command", 0),
        ("infrastructure_wide_outage", 0),
        ("missing_fingerprint", 0),
    ]);
    let mut canonical_build_executions = 0;
    let mut fingerprints_by_cohort: BTreeMap<String, BTreeSet<(String, String, String)>> =
        BTreeMap::new();
    let mut completed_runs_by_cohort = BTreeMap::new();

    for event in events {
        if event.event_kind == "reuse"
            && event.stored_fingerprint.as_deref() != event.submitted_c2_fingerprint.as_deref()
        {
            return Err(format!(
                "audited reuse {} stored fingerprint differs from submission-time C2",
                event.id
            ));
        }
        let Some(cohort) = cohort_for(metadata, &event.timestamp) else {
            continue;
        };
        if let Some(reason) = exclusion(metadata, event) {
            *exclusions.get_mut(reason).expect("known exclusion") += 1;
            continue;
        }

        let fingerprint = event
            .verification_input_fingerprint
            .as_ref()
            .expect("included events have fingerprints");
        fingerprints_by_cohort
            .entry(cohort.name.clone())
            .or_default()
            .insert((
                event.project_id.clone(),
                event.task_id.clone(),
                fingerprint.clone(),
            ));
        if event.completed_task_run {
            *completed_runs_by_cohort
                .entry(cohort.name.clone())
                .or_insert(0) += 1;
        }
        if event.event_kind == "canonical" && event.executes_build_command {
            canonical_build_executions += 1;
        }
    }

    let mut cohorts = BTreeMap::new();
    for cohort in &metadata.cohorts {
        let completed_task_runs = completed_runs_by_cohort.remove(&cohort.name).unwrap_or(0);
        let distinct_fingerprints = fingerprints_by_cohort
            .remove(&cohort.name)
            .map_or(0, |fingerprints| fingerprints.len());
        if completed_task_runs < metadata.minimum_completed_task_runs_per_cohort {
            return Err(format!(
                "cohort {} has {completed_task_runs} completed task runs; minimum is {}",
                cohort.name, metadata.minimum_completed_task_runs_per_cohort
            ));
        }
        if distinct_fingerprints < metadata.minimum_distinct_fingerprints_per_cohort {
            return Err(format!(
                "cohort {} has {distinct_fingerprints} fingerprints; minimum is {}",
                cohort.name, metadata.minimum_distinct_fingerprints_per_cohort
            ));
        }
        cohorts.insert(
            cohort.name.clone(),
            CohortReport {
                completed_task_runs,
                distinct_fingerprints,
            },
        );
    }

    let distinct_fingerprints = cohorts
        .values()
        .map(|cohort| cohort.distinct_fingerprints)
        .sum();
    let ratio = validate_ratio(
        canonical_build_executions,
        distinct_fingerprints,
        metadata.ratio_must_be_less_than,
    )?;

    Ok(Report {
        query_version: metadata.query_version.clone(),
        canonical_build_executions,
        distinct_fingerprints,
        ratio,
        cohorts,
        exclusions,
    })
}

fn fixtures() -> (Metadata, Fixture) {
    (
        serde_json::from_str(METADATA).expect("valid report metadata"),
        serde_json::from_str(EVENTS).expect("valid report events"),
    )
}

#[test]
fn fixture_report_publishes_qualified_cohorts_exclusions_and_exact_arithmetic() {
    let (metadata, fixture) = fixtures();
    assert_eq!(metadata.query_version, "verify_dedup_report_v1");
    assert_eq!(metadata.cohorts.len(), 2);
    assert!(
        metadata.cohorts[0].end_exclusive <= metadata.cohorts[1].start_inclusive,
        "pre and post windows must not overlap"
    );
    assert!(QUERY.contains("verify_dedup_report_v1"));
    assert!(
        QUERY.contains("COUNT(DISTINCT (project_id, task_id, verification_input_fingerprint))")
    );
    assert!(QUERY.contains("declared_infrastructure_wide_outages"));
    assert!(QUERY.contains("event_timestamp >= outage.start_inclusive"));
    assert!(QUERY.contains("event_timestamp < outage.end_exclusive"));

    let outage_event = fixture
        .events
        .iter()
        .find(|event| event.id == "excluded-outage-inside-declared-interval")
        .expect("fixture event inside declared outage");
    let outage_control = fixture
        .events
        .iter()
        .find(|event| event.id == "outage-control-outside-declared-interval")
        .expect("fixture control event outside declared outage");
    assert!(
        !outage_event.infrastructure_wide_outage,
        "outage classification must come from metadata, not an event-local flag"
    );
    assert!(is_declared_outage(&metadata, &outage_event.timestamp));
    assert_eq!(
        exclusion(&metadata, outage_event),
        Some("infrastructure_wide_outage")
    );
    assert!(
        !is_declared_outage(&metadata, &outage_control.timestamp),
        "control outside the half-open interval must remain eligible"
    );
    assert_eq!(exclusion(&metadata, outage_control), None);

    let report =
        calculate_report(&metadata, &fixture.events).expect("below-boundary fixture passes");
    assert_eq!(report.query_version, "verify_dedup_report_v1");
    assert_eq!(report.canonical_build_executions, 87);
    assert_eq!(report.distinct_fingerprints, 60);
    assert!((report.ratio - 1.45).abs() < f64::EPSILON);
    for name in ["pre", "post"] {
        let cohort = report.cohorts.get(name).expect("published cohort");
        assert_eq!(cohort.completed_task_runs, 50);
        assert_eq!(cohort.distinct_fingerprints, 30);
    }
    assert_eq!(report.exclusions["project_ci"], 1);
    assert_eq!(report.exclusions["merge_queue_ci"], 1);
    assert_eq!(report.exclusions["cancelled_before_first_command"], 1);
    assert_eq!(report.exclusions["infrastructure_wide_outage"], 1);
    assert_eq!(report.exclusions["missing_fingerprint"], 1);
}

#[test]
fn ratio_exactly_one_point_five_is_rejected_and_below_is_accepted() {
    let (metadata, fixture) = fixtures();
    let exact = fixture
        .boundary_cases
        .get("exactly_1_5")
        .expect("exact case");
    let below = fixture.boundary_cases.get("below_1_5").expect("below case");
    let exact_ratio = exact.canonical_build_executions as f64 / exact.distinct_fingerprints as f64;
    let below_ratio = below.canonical_build_executions as f64 / below.distinct_fingerprints as f64;
    assert_eq!(exact_ratio, 1.5);
    assert!(
        validate_ratio(
            exact.canonical_build_executions,
            exact.distinct_fingerprints,
            metadata.ratio_must_be_less_than,
        )
        .expect_err("ratio exactly 1.5 must fail")
        .contains("must be below")
    );
    assert_eq!(
        validate_ratio(
            below.canonical_build_executions,
            below.distinct_fingerprints,
            metadata.ratio_must_be_less_than,
        )
        .expect("ratio below 1.5 must pass"),
        below_ratio
    );
}

#[test]
fn audited_reuse_with_c2_fingerprint_mismatch_fails_closed_regardless_of_ratio() {
    let (metadata, mut fixture) = fixtures();
    let reuse = fixture
        .events
        .iter_mut()
        .find(|event| event.event_kind == "reuse" && !event.project_ci)
        .expect("eligible reuse encounter");
    reuse.stored_fingerprint = Some("stored-fingerprint-from-another-input".into());
    let error =
        calculate_report(&metadata, &fixture.events).expect_err("mismatched audit must fail");
    assert!(error.contains("submission-time C2"));
}
