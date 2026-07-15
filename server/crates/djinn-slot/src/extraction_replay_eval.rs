//! Deterministic, serializable result model for offline extraction replay.
//!
//! The runner owns transcript/database setup; this module deliberately owns only
//! fixture annotations, captured production decisions, and rubric scoring.

use std::collections::BTreeMap;

use djinn_db::assess_note_quality;
use serde::{Deserialize, Serialize};

/// Annotation supplied by an archived-transcript replay fixture.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtractionReplayFixture {
    pub id: String,
    /// Text which must occur in an admitted extraction.
    pub required_discriminative_text: String,
    pub expected_note_type: String,
    pub expect_adr_054_quality: bool,
    /// Existing note ID that this fixture must not be merged into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub must_not_duplicate_target: Option<String>,
    /// Optional positive duplicate expectation, used when evaluating dedup precision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_duplicate_target: Option<String>,
}

/// A non-persisted decision captured from the extraction path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtractionObservation {
    pub fixture_id: String,
    pub note_type: String,
    pub title: String,
    pub content: String,
    /// Result of the shared ADR-054 classifier at capture time.
    pub adr_054_quality_passed: bool,
    /// Candidate selected by the production novelty decision, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duplicate_of: Option<String>,
}

/// Satisfaction for one fixture dimension.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplayRubricSatisfaction {
    pub required_text: bool,
    pub note_type: bool,
    pub adr_054_quality: bool,
    pub must_not_duplicate: bool,
}

impl ReplayRubricSatisfaction {
    fn all(&self) -> bool {
        self.required_text && self.note_type && self.adr_054_quality && self.must_not_duplicate
    }
}

/// Stable failure information suitable for human and machine output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplayFailureDiagnostic {
    pub fixture_id: String,
    pub dimension: String,
}

/// Scored result for a single fixture.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtractionReplayCaseResult {
    pub fixture_id: String,
    pub rubric: ReplayRubricSatisfaction,
    pub failed_dimensions: Vec<String>,
    pub observations: Vec<ExtractionObservation>,
}

/// Confusion counts for the duplicate prediction. Precision is zero when there
/// are no predicted duplicates; this deliberately avoids reporting a vacuous 1.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DedupConfusionCounts {
    pub true_positive: u32,
    pub false_positive: u32,
    pub true_negative: u32,
    pub false_negative: u32,
}

impl DedupConfusionCounts {
    pub fn precision(&self) -> f64 {
        let denominator = self.true_positive + self.false_positive;
        if denominator == 0 {
            0.0
        } else {
            f64::from(self.true_positive) / f64::from(denominator)
        }
    }
}

/// Aggregate deterministic replay report.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExtractionReplayReport {
    pub cases: Vec<ExtractionReplayCaseResult>,
    pub total_cases: u32,
    pub satisfied_cases: u32,
    pub rubric_satisfaction_rate: f64,
    pub dedup: DedupConfusionCounts,
    pub dedup_precision: f64,
    pub failures: Vec<ReplayFailureDiagnostic>,
}

/// Score captured observations. Quality is deliberately re-evaluated through
/// `assess_note_quality`, rather than trusting a fixture-side classifier.
pub fn score_extraction_replay(
    fixtures: &[ExtractionReplayFixture],
    observations: &[ExtractionObservation],
) -> ExtractionReplayReport {
    let mut by_fixture: BTreeMap<&str, Vec<ExtractionObservation>> = BTreeMap::new();
    for observation in observations {
        by_fixture
            .entry(observation.fixture_id.as_str())
            .or_default()
            .push(observation.clone());
    }

    let mut fixtures = fixtures.to_vec();
    fixtures.sort_by(|left, right| left.id.cmp(&right.id));
    let mut cases = Vec::with_capacity(fixtures.len());
    let mut failures = Vec::new();
    let mut dedup = DedupConfusionCounts::default();

    for fixture in fixtures {
        let captured = by_fixture.remove(fixture.id.as_str()).unwrap_or_default();
        let required_text = captured.iter().any(|observation| {
            observation
                .content
                .contains(&fixture.required_discriminative_text)
        });
        let note_type = captured
            .iter()
            .any(|observation| observation.note_type == fixture.expected_note_type);
        let adr_054_quality = captured.iter().any(|observation| {
            observation.note_type == fixture.expected_note_type
                && (!assess_note_quality(&observation.note_type, &observation.content)
                    .is_underspecified)
                    == fixture.expect_adr_054_quality
        });
        let must_not_duplicate = fixture
            .must_not_duplicate_target
            .as_ref()
            .is_none_or(|target| {
                captured
                    .iter()
                    .all(|observation| observation.duplicate_of.as_deref() != Some(target.as_str()))
            });

        let predicted_target = captured
            .iter()
            .find_map(|observation| observation.duplicate_of.as_deref());
        let expected_target = fixture.expected_duplicate_target.as_deref();
        match (predicted_target, expected_target) {
            (Some(predicted), Some(expected)) if predicted == expected => dedup.true_positive += 1,
            (Some(_), Some(_)) => {
                dedup.false_positive += 1;
                dedup.false_negative += 1;
            }
            (Some(_), None) => dedup.false_positive += 1,
            (None, Some(_)) => dedup.false_negative += 1,
            (None, None) => dedup.true_negative += 1,
        }

        let rubric = ReplayRubricSatisfaction {
            required_text,
            note_type,
            adr_054_quality,
            must_not_duplicate,
        };
        let mut failed_dimensions = Vec::new();
        for (dimension, passed) in [
            ("adr_054_quality", rubric.adr_054_quality),
            ("must_not_duplicate", rubric.must_not_duplicate),
            ("note_type", rubric.note_type),
            ("required_discriminative_text", rubric.required_text),
        ] {
            if !passed {
                failed_dimensions.push(dimension.to_string());
                failures.push(ReplayFailureDiagnostic {
                    fixture_id: fixture.id.clone(),
                    dimension: dimension.to_string(),
                });
            }
        }
        cases.push(ExtractionReplayCaseResult {
            fixture_id: fixture.id,
            rubric,
            failed_dimensions,
            observations: captured,
        });
    }

    let total_cases = cases.len() as u32;
    let satisfied_cases = cases.iter().filter(|case| case.rubric.all()).count() as u32;
    let rubric_satisfaction_rate = if total_cases == 0 {
        0.0
    } else {
        f64::from(satisfied_cases) / f64::from(total_cases)
    };
    let dedup_precision = dedup.precision();
    ExtractionReplayReport {
        cases,
        total_cases,
        satisfied_cases,
        rubric_satisfaction_rate,
        dedup,
        dedup_precision,
        failures,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(id: &str) -> ExtractionReplayFixture {
        ExtractionReplayFixture {
            id: id.to_string(),
            required_discriminative_text: "stable candidate seam".to_string(),
            expected_note_type: "case".to_string(),
            expect_adr_054_quality: false,
            must_not_duplicate_target: None,
            expected_duplicate_target: None,
        }
    }

    #[test]
    fn zero_predicted_duplicates_has_zero_precision() {
        assert_eq!(DedupConfusionCounts::default().precision(), 0.0);
    }

    #[test]
    fn report_and_failure_dimensions_are_stably_sorted() {
        let fixtures = vec![fixture("z-case"), fixture("a-case")];
        let report = score_extraction_replay(&fixtures, &[]);
        assert_eq!(report.cases[0].fixture_id, "a-case");
        assert_eq!(report.failures[0].fixture_id, "a-case");
        assert_eq!(
            report.cases[0].failed_dimensions,
            vec![
                "adr_054_quality",
                "note_type",
                "required_discriminative_text"
            ]
        );
        assert_eq!(report.total_cases, 2);
        assert_eq!(report.rubric_satisfaction_rate, 0.0);
    }

    #[test]
    fn dedup_counts_false_duplicate_prediction() {
        let mut fixture = fixture("case");
        fixture.must_not_duplicate_target = Some("live-note".to_string());
        let observation = ExtractionObservation {
            fixture_id: "case".to_string(),
            note_type: "case".to_string(),
            title: "candidate".to_string(),
            content: "stable candidate seam".to_string(),
            adr_054_quality_passed: false,
            duplicate_of: Some("live-note".to_string()),
        };
        let report = score_extraction_replay(&[fixture], &[observation]);
        assert_eq!(report.dedup.false_positive, 1);
        assert_eq!(report.dedup_precision, 0.0);
        assert_eq!(report.failures[0].dimension, "must_not_duplicate");
    }

    #[test]
    fn dedup_counts_true_duplicate_prediction() {
        let mut fixture = fixture("case");
        fixture.expected_duplicate_target = Some("existing-note".to_string());
        let observation = ExtractionObservation {
            fixture_id: "case".to_string(),
            note_type: "case".to_string(),
            title: "candidate".to_string(),
            content: "stable candidate seam".to_string(),
            adr_054_quality_passed: false,
            duplicate_of: Some("existing-note".to_string()),
        };
        let report = score_extraction_replay(&[fixture], &[observation]);
        assert_eq!(report.dedup.true_positive, 1);
        assert_eq!(report.dedup_precision, 1.0);
    }
}
