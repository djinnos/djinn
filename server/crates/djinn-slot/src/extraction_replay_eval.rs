//! Deterministic, serializable result model for offline extraction replay.
//!
//! The runner owns transcript/database setup; this module deliberately owns only
//! fixture annotations, captured production decisions, and rubric scoring.

use std::collections::BTreeMap;

use async_trait::async_trait;
use djinn_core::{events::EventBus, message::Conversation};
use djinn_db::{
    Database, NoteDedupCandidate, NoteRepository, SessionMessageRepository, assess_note_quality,
    folder_for_type,
};
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

/// Load transcripts through the production message repository, perform the
/// production candidate lookup, and invoke an injected capture seam. Errors
/// remain attached to their fixture and stay in the aggregate denominator.
pub async fn run_database_extraction_replay(
    db: Database,
    events: EventBus,
    eval_project_id: &str,
    cases: &[DatabaseReplayCase],
    seam: &dyn ReplayExtractionSeam,
) -> ExtractionReplayReport {
    let messages = SessionMessageRepository::new(db.clone(), events.clone());
    let notes = NoteRepository::new(db, events);
    let mut observations = Vec::new();
    let mut stage_failures: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for case in cases {
        let fixture_id = case.fixture.id.clone();
        let transcript = match messages.load_conversation(&case.session_id).await {
            Ok(transcript) => transcript,
            Err(_) => {
                stage_failures
                    .entry(fixture_id)
                    .or_default()
                    .push("repository.load_conversation".to_string());
                continue;
            }
        };
        let lookup_text = if case.candidate_lookup_text.is_empty() {
            case.fixture.required_discriminative_text.as_str()
        } else {
            case.candidate_lookup_text.as_str()
        };
        let candidates = match notes
            .dedup_candidates(
                eval_project_id,
                folder_for_type(&case.fixture.expected_note_type),
                &case.fixture.expected_note_type,
                lookup_text,
                3,
            )
            .await
        {
            Ok(candidates) => candidates,
            Err(_) => {
                stage_failures
                    .entry(fixture_id)
                    .or_default()
                    .push("repository.dedup_candidates".to_string());
                continue;
            }
        };
        match seam
            .capture(&case.fixture.id, &transcript, &candidates)
            .await
        {
            Ok(captured) => observations.extend(captured),
            Err(_) => stage_failures
                .entry(case.fixture.id.clone())
                .or_default()
                .push("provider.capture".to_string()),
        }
    }

    let fixtures: Vec<_> = cases.iter().map(|case| case.fixture.clone()).collect();
    let mut report = score_extraction_replay(&fixtures, &observations);
    for case in &mut report.cases {
        if let Some(stages) = stage_failures.get(&case.fixture_id) {
            case.failed_stages = stages.clone();
            for stage in stages {
                report.failures.push(ReplayFailureDiagnostic {
                    fixture_id: case.fixture_id.clone(),
                    dimension: stage.clone(),
                });
            }
        }
    }
    report.satisfied_cases = report
        .cases
        .iter()
        .filter(|case| case.rubric.all() && case.failed_stages.is_empty())
        .count() as u32;
    report.rubric_satisfaction_rate = if report.total_cases == 0 {
        0.0
    } else {
        f64::from(report.satisfied_cases) / f64::from(report.total_cases)
    };
    report
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
    /// Infrastructure stages that failed before a fixture could be scored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_stages: Vec<String>,
    pub observations: Vec<ExtractionObservation>,
}

/// One persisted transcript to replay. The session must belong to the supplied
/// disposable evaluation project; the runner never resolves ambient state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatabaseReplayCase {
    pub fixture: ExtractionReplayFixture,
    pub session_id: String,
    /// Text passed to the production repository candidate lookup.
    #[serde(default)]
    pub candidate_lookup_text: String,
}

/// Injectable production-path capture seam. The runner supplies the persisted
/// transcript and candidates returned by `NoteRepository::dedup_candidates`.
#[async_trait]
pub trait ReplayExtractionSeam: Send + Sync {
    async fn capture(
        &self,
        fixture_id: &str,
        transcript: &Conversation,
        candidates: &[NoteDedupCandidate],
    ) -> Result<Vec<ExtractionObservation>, String>;
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
            failed_stages: Vec::new(),
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
    use std::sync::{Arc, Mutex};

    use djinn_core::{events::EventBus, message::ContentBlock};
    use djinn_db::{CreateSessionParams, ProjectRepository, SessionRepository, TaskRepository};

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

    #[derive(Default)]
    struct RecordingCaptureSeam {
        transcripts: Mutex<Vec<String>>,
        candidate_ids: Mutex<Vec<String>>,
        fail: bool,
    }

    #[async_trait]
    impl ReplayExtractionSeam for RecordingCaptureSeam {
        async fn capture(
            &self,
            fixture_id: &str,
            transcript: &Conversation,
            candidates: &[NoteDedupCandidate],
        ) -> Result<Vec<ExtractionObservation>, String> {
            if self.fail {
                return Err("deterministic provider failure".to_string());
            }
            self.transcripts
                .lock()
                .unwrap()
                .push(format!("{transcript:?}"));
            self.candidate_ids
                .lock()
                .unwrap()
                .extend(candidates.iter().map(|candidate| candidate.id.clone()));
            Ok(vec![ExtractionObservation {
                fixture_id: fixture_id.to_string(),
                note_type: "case".to_string(),
                title: "replayed extraction".to_string(),
                content: "stable candidate seam".to_string(),
                adr_054_quality_passed: false,
                duplicate_of: None,
            }])
        }
    }

    async fn create_replay_session(
        db: &Database,
        events: EventBus,
        project_id: &str,
        transcript_text: &str,
    ) -> String {
        let task = TaskRepository::new(db.clone(), events.clone())
            .create_in_project(
                project_id,
                None,
                "replay task",
                "replay fixture task",
                "",
                "task",
                1,
                "eval",
                None,
                None,
            )
            .await
            .unwrap();
        let session = SessionRepository::new(db.clone(), events.clone())
            .create(CreateSessionParams {
                project_id,
                task_id: Some(&task.id),
                model: "deterministic/replay",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();
        SessionMessageRepository::new(db.clone(), events)
            .insert_messages_batch(
                &session.id,
                &task.id,
                &[djinn_core::message::Message {
                    role: djinn_core::message::Role::User,
                    content: vec![ContentBlock::text(transcript_text)],
                    metadata: None,
                }],
            )
            .await
            .unwrap();
        session.id
    }

    #[tokio::test]
    async fn database_replay_uses_persisted_transcripts_candidates_and_isolates_live_corpus() {
        let db = Database::open_in_memory().unwrap();
        let events = EventBus::noop();
        let projects = ProjectRepository::new(db.clone(), events.clone());
        let live = projects.create("live", "test", "live").await.unwrap();
        let eval = projects.create("eval", "test", "eval").await.unwrap();
        let notes = NoteRepository::new(db.clone(), events.clone());
        let sentinel = notes
            .create_db_note_with_permalink(
                &live.id,
                "sentinel",
                "live sentinel",
                "byte-for-byte live corpus sentinel",
                "case",
                "[]",
            )
            .await
            .unwrap();
        let candidate = notes
            .create_db_note_with_permalink(
                &eval.id,
                "candidate",
                "eval candidate",
                "stable candidate seam",
                "case",
                "[]",
            )
            .await
            .unwrap();
        let before = serde_json::to_vec(&notes.list(&live.id, None).await.unwrap()).unwrap();
        let eval_note_count_before = notes.count_by_project(&eval.id).await.unwrap();
        let session_id =
            create_replay_session(&db, events.clone(), &eval.id, "fixture transcript").await;
        let mut replay_fixture = fixture("db-case");
        replay_fixture.must_not_duplicate_target = Some(candidate.id.clone());
        let seam = Arc::new(RecordingCaptureSeam::default());
        let report = run_database_extraction_replay(
            db.clone(),
            events,
            &eval.id,
            &[DatabaseReplayCase {
                fixture: replay_fixture,
                session_id,
                candidate_lookup_text: "stable candidate seam".to_string(),
            }],
            seam.as_ref(),
        )
        .await;

        assert_eq!(report.total_cases, 1);
        assert_eq!(report.satisfied_cases, 1);
        assert!(seam.transcripts.lock().unwrap()[0].contains("fixture transcript"));
        assert!(seam.candidate_ids.lock().unwrap().contains(&candidate.id));
        let after_notes = notes.list(&live.id, None).await.unwrap();
        let after = serde_json::to_vec(&after_notes).unwrap();
        assert_eq!(before, after);
        assert_eq!(after_notes.len(), 1);
        assert_eq!(after_notes[0].id, sentinel.id);
        assert_eq!(
            notes.count_by_project(&eval.id).await.unwrap(),
            eval_note_count_before
        );
    }

    #[tokio::test]
    async fn provider_failure_stays_in_fixture_report() {
        let db = Database::open_in_memory().unwrap();
        let events = EventBus::noop();
        let eval = ProjectRepository::new(db.clone(), events.clone())
            .create("eval", "test", "provider-failure")
            .await
            .unwrap();
        let session_id =
            create_replay_session(&db, events.clone(), &eval.id, "failure fixture").await;
        let seam = RecordingCaptureSeam {
            fail: true,
            ..Default::default()
        };
        let report = run_database_extraction_replay(
            db,
            events,
            &eval.id,
            &[DatabaseReplayCase {
                fixture: fixture("provider-failure"),
                session_id,
                candidate_lookup_text: String::new(),
            }],
            &seam,
        )
        .await;
        assert_eq!(report.total_cases, 1);
        assert_eq!(report.satisfied_cases, 0);
        assert_eq!(report.cases[0].failed_stages, vec!["provider.capture"]);
        assert!(
            report
                .failures
                .iter()
                .any(|failure| failure.dimension == "provider.capture")
        );
    }
}
