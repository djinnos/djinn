//! Deterministic, serializable result model for offline extraction replay.
//!
//! The runner owns transcript/database setup; this module deliberately owns only
//! fixture annotations, captured production decisions, and rubric scoring.

use std::collections::BTreeMap;
use std::path::Path;
#[cfg(feature = "test-support")]
use std::{
    collections::VecDeque,
    pin::Pin,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use djinn_core::{events::EventBus, message::Conversation};
use djinn_db::{
    Database, NoteDedupCandidate, NoteRepository, SessionMessageRepository, assess_note_quality,
    folder_for_type,
};
#[cfg(feature = "test-support")]
use futures::{Future, Stream};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplayTranscriptMessage {
    pub role: String,
    pub content: String,
}

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
    #[serde(default)]
    pub provenance: String,
    #[serde(default)]
    pub messages: Vec<ReplayTranscriptMessage>,
    #[serde(default)]
    pub terminal_context: crate::llm_extraction::TerminalExtractionContext,
    #[serde(default)]
    pub injected_provider_response: String,
}

/// Load the committed corpus in stable filename order and reject unsafe rows.
pub fn load_extraction_replay_fixtures(
    directory: impl AsRef<Path>,
) -> Result<Vec<ExtractionReplayFixture>, String> {
    let mut paths = std::fs::read_dir(directory.as_ref())
        .map_err(|error| error.to_string())?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    paths.sort();
    let fixtures = paths
        .into_iter()
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .map(|path| {
            serde_json::from_str(
                &std::fs::read_to_string(&path).map_err(|error| error.to_string())?,
            )
            .map_err(|error| format!("{}: {error}", path.display()))
        })
        .collect::<Result<Vec<ExtractionReplayFixture>, String>>()?;
    validate_extraction_replay_fixtures(&fixtures)?;
    Ok(fixtures)
}

/// Enforce typed replay, dedup-target, ADR-054, and repository-safety contracts.
pub fn validate_extraction_replay_fixtures(
    fixtures: &[ExtractionReplayFixture],
) -> Result<(), String> {
    if !(20..=50).contains(&fixtures.len()) {
        return Err(format!(
            "fixture count {} is outside 20..=50",
            fixtures.len()
        ));
    }

    let mut ids = std::collections::BTreeSet::new();
    for fixture in fixtures {
        if fixture.id.trim().is_empty() || !ids.insert(fixture.id.as_str()) {
            return Err(format!("missing or duplicate ID: {}", fixture.id));
        }
        if fixture.required_discriminative_text.trim().is_empty() {
            return Err(format!("{} has empty discriminative fact", fixture.id));
        }
        if !matches!(
            fixture.expected_note_type.as_str(),
            "case" | "pattern" | "pitfall"
        ) {
            return Err(format!("{} has unsupported note type", fixture.id));
        }
        if fixture.messages.is_empty()
            || fixture.messages.iter().any(|message| {
                !matches!(
                    message.role.as_str(),
                    "system" | "user" | "assistant" | "tool"
                ) || message.content.trim().is_empty()
            })
        {
            return Err(format!("{} has malformed messages", fixture.id));
        }
        if fixture.provenance != "sanitized_archived_transcript_derived" {
            return Err(format!("{} has unsafe provenance", fixture.id));
        }
        let serialized = serde_json::to_string(fixture).map_err(|error| error.to_string())?;
        let lower = serialized.to_ascii_lowercase();
        if [
            "api_key",
            "password",
            "secret",
            "bearer ",
            "sk-",
            "session-",
            "postgres://",
            "production",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
            || serialized.contains("T00:")
        {
            return Err(format!("{} contains prohibited data", fixture.id));
        }
        let response: serde_json::Value = serde_json::from_str(&fixture.injected_provider_response)
            .map_err(|_| format!("{} has malformed response", fixture.id))?;
        let response_key = match fixture.expected_note_type.as_str() {
            "case" => "cases",
            "pattern" => "patterns",
            "pitfall" => "pitfalls",
            _ => unreachable!("note type was validated above"),
        };
        let content = response
            .get(response_key)
            .and_then(serde_json::Value::as_array)
            .and_then(|notes| notes.first())
            .and_then(|note| note.get("content"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("{} response lacks accepted note", fixture.id))?;
        if !content.contains(&fixture.required_discriminative_text)
            || (!assess_note_quality(&fixture.expected_note_type, content).is_underspecified)
                != fixture.expect_adr_054_quality
        {
            return Err(format!("{} response cannot satisfy rubric", fixture.id));
        }
        for target in [
            &fixture.must_not_duplicate_target,
            &fixture.expected_duplicate_target,
        ]
        .into_iter()
        .flatten()
        {
            if target.trim().is_empty() || !target.starts_with("candidate-") {
                return Err(format!("{} has unresolved dedup target", fixture.id));
            }
        }
    }
    Ok(())
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

/// Production-path replay seam that drives the full extraction + novelty
/// pipeline through [`capture_llm_extraction_replay`].
///
/// The extraction provider receives the transcript and returns the extraction
/// JSON (as production `run_llm_extraction_inner` would). That response is fed
/// to [`capture_llm_extraction_replay`] alongside the same deterministic
/// novelty provider and the repository-returned candidates. This reuses the
/// production parser, intra-batch dedup, ADR-054 gate, and novelty
/// request/response contract — stopping before persistence.
#[cfg(any(test, feature = "test-support"))]
pub struct ProductionReplaySeam {
    /// Provider for the extraction completion (transcript → extraction JSON).
    pub extraction_provider: std::sync::Arc<dyn djinn_provider::provider::LlmProvider>,
    /// Provider for the novelty decision (proposed note vs candidates).
    pub novelty_provider: std::sync::Arc<dyn djinn_provider::provider::LlmProvider>,
}

#[cfg(any(test, feature = "test-support"))]
#[async_trait]
impl ReplayExtractionSeam for ProductionReplaySeam {
    async fn capture(
        &self,
        fixture_id: &str,
        transcript: &Conversation,
        candidates: &[NoteDedupCandidate],
    ) -> Result<Vec<ExtractionObservation>, String> {
        let transcript_text = transcript
            .messages
            .iter()
            .filter_map(|msg| {
                use djinn_core::message::ContentBlock;
                let text: String = msg
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if text.is_empty() { None } else { Some(text) }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let extraction_prompt =
            format!("Transcript:\n{transcript_text}\n\nExtract knowledge notes as JSON.");
        let extraction_response = djinn_provider::complete(
            self.extraction_provider.as_ref(),
            djinn_provider::CompletionRequest {
                system: "You are a knowledge extractor. Respond with valid JSON only.".to_string(),
                prompt: extraction_prompt,
                max_tokens: 4096,
            },
        )
        .await
        .map_err(|e| format!("extraction completion failed: {e}"))?;
        crate::capture_llm_extraction_replay(
            fixture_id.to_string(),
            &extraction_response.text,
            self.novelty_provider.as_ref(),
            candidates,
        )
        .await
    }
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

/// Rubric floors enforced by the offline command. Both values are fractions in
/// the inclusive range `0.0..=1.0`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct OfflineReplayThresholds {
    pub minimum_rubric_satisfaction: f64,
    pub minimum_dedup_precision: f64,
}

impl Default for OfflineReplayThresholds {
    fn default() -> Self {
        Self {
            minimum_rubric_satisfaction: 1.0,
            minimum_dedup_precision: 1.0,
        }
    }
}

impl OfflineReplayThresholds {
    pub fn validate(self) -> Result<(), String> {
        for (name, value) in [
            (
                "minimum_rubric_satisfaction",
                self.minimum_rubric_satisfaction,
            ),
            ("minimum_dedup_precision", self.minimum_dedup_precision),
        ] {
            if !(0.0..=1.0).contains(&value) {
                return Err(format!("{name} must be within 0.0..=1.0"));
            }
        }
        Ok(())
    }

    pub fn unmet_dimensions(self, report: &ExtractionReplayReport) -> Vec<String> {
        let mut unmet = Vec::new();
        if report.rubric_satisfaction_rate < self.minimum_rubric_satisfaction {
            unmet.push("rubric_satisfaction".to_string());
        }
        if report.dedup_precision < self.minimum_dedup_precision {
            unmet.push("dedup_precision".to_string());
        }
        unmet
    }
}

/// Render the stable human-readable companion to the JSON report.
pub fn render_extraction_replay_markdown(
    report: &ExtractionReplayReport,
    thresholds: OfflineReplayThresholds,
) -> String {
    let mut output = format!(
        "# Offline extraction replay report\n\n\
         - Cases: {}/{} ({:.4})\n\
         - Dedup precision: {:.4}\n\
         - Dedup confusion: TP={} FP={} TN={} FN={}\n\
         - Thresholds: rubric >= {:.4}; dedup precision >= {:.4}\n\n\
         ## Per fixture\n\n",
        report.satisfied_cases,
        report.total_cases,
        report.rubric_satisfaction_rate,
        report.dedup_precision,
        report.dedup.true_positive,
        report.dedup.false_positive,
        report.dedup.true_negative,
        report.dedup.false_negative,
        thresholds.minimum_rubric_satisfaction,
        thresholds.minimum_dedup_precision,
    );
    for case in &report.cases {
        let mut failed = case.failed_dimensions.clone();
        failed.extend(case.failed_stages.iter().cloned());
        let result = if failed.is_empty() { "PASS" } else { "FAIL" };
        let failed = if failed.is_empty() {
            "none".to_string()
        } else {
            failed.join(", ")
        };
        output.push_str(&format!(
            "- **{result}** `{}` — failed: {failed}\n",
            case.fixture_id
        ));
    }
    let unmet = thresholds.unmet_dimensions(report);
    if !unmet.is_empty() {
        output.push_str(&format!(
            "\n## Gate failure\n\nThresholds not met: {}\n",
            unmet.join(", ")
        ));
    }
    output
}

/// Replace disposable database candidate IDs with stable fixture labels before
/// an offline report is serialized. The production novelty path continues to
/// use database IDs returned by the repository candidate lookup; translation
/// happens only at this reporting boundary.
#[cfg(any(test, feature = "test-support"))]
fn normalize_offline_duplicate_targets(
    report: &mut ExtractionReplayReport,
    stable_target_by_database_id: &BTreeMap<String, String>,
) {
    for case in &mut report.cases {
        for observation in &mut case.observations {
            if let Some(stable_target) = observation
                .duplicate_of
                .as_ref()
                .and_then(|database_id| stable_target_by_database_id.get(database_id))
            {
                observation.duplicate_of = Some(stable_target.clone());
            }
        }
    }
}

#[cfg(feature = "test-support")]
struct FixtureResponseProvider {
    responses: Mutex<VecDeque<String>>,
}

#[cfg(feature = "test-support")]
impl FixtureResponseProvider {
    fn new(responses: Vec<String>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
        }
    }
}

/// Queue-backed fixtures cannot resolve a client, inspect credentials, or make
/// a network request.
#[cfg(feature = "test-support")]
impl djinn_provider::provider::LlmProvider for FixtureResponseProvider {
    fn name(&self) -> &str {
        "offline-fixture"
    }
    fn stream<'a>(
        &'a self,
        _conversation: &'a djinn_provider::message::Conversation,
        _tools: &'a [serde_json::Value],
        _tool_choice: Option<djinn_provider::provider::ToolChoice>,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = anyhow::Result<
                        Pin<
                            Box<
                                dyn Stream<
                                        Item = anyhow::Result<
                                            djinn_provider::provider::StreamEvent,
                                        >,
                                    > + Send,
                            >,
                        >,
                    >,
                > + Send
                + 'a,
        >,
    > {
        let response = self
            .responses
            .lock()
            .map_err(|_| anyhow::anyhow!("offline fixture response queue poisoned"))
            .map(|mut queue| queue.pop_front().unwrap_or_default());
        Box::pin(async move {
            let stream = futures::stream::iter(vec![
                Ok(djinn_provider::provider::StreamEvent::Delta(
                    djinn_core::message::ContentBlock::text(response?),
                )),
                Ok(djinn_provider::provider::StreamEvent::Done),
            ]);
            Ok(Box::pin(stream) as _)
        })
    }
}

/// Execute the committed corpus against a template-cloned test Postgres
/// database using only fixture-backed providers. No production project opens.
#[cfg(feature = "test-support")]
pub async fn run_offline_fixture_replay(
    fixture_directory: impl AsRef<Path>,
) -> Result<ExtractionReplayReport, String> {
    let fixtures = load_extraction_replay_fixtures(fixture_directory)?;
    let db = Database::open_in_memory().map_err(|error| error.to_string())?;
    let events = EventBus::noop();
    let eval = djinn_db::ProjectRepository::new(db.clone(), events.clone())
        .create("extraction-replay", "test", "offline-fixture-replay")
        .await
        .map_err(|error| error.to_string())?;
    let notes = NoteRepository::new(db.clone(), events.clone());
    let mut cases = Vec::with_capacity(fixtures.len());
    // Fixture target labels are stable report values, while note creation
    // assigns disposable UUIDs required by production-path candidate matching.
    let mut stable_target_by_database_id = BTreeMap::new();
    for mut fixture in fixtures {
        for target in [
            &mut fixture.expected_duplicate_target,
            &mut fixture.must_not_duplicate_target,
        ]
        .into_iter()
        .flatten()
        {
            let candidate = notes
                .create_db_note_with_permalink(
                    &eval.id,
                    target,
                    target,
                    &fixture.required_discriminative_text,
                    &fixture.expected_note_type,
                    "[]",
                )
                .await
                .map_err(|error| error.to_string())?;
            stable_target_by_database_id.insert(candidate.id.clone(), target.clone());
            *target = candidate.id;
        }
        let task = djinn_db::TaskRepository::new(db.clone(), events.clone())
            .create_in_project(
                &eval.id,
                None,
                &format!("replay {}", fixture.id),
                "offline replay fixture",
                "",
                "task",
                1,
                "eval",
                None,
                None,
            )
            .await
            .map_err(|error| error.to_string())?;
        let metadata =
            serde_json::to_string(&fixture.terminal_context).map_err(|error| error.to_string())?;
        let session = djinn_db::SessionRepository::new(db.clone(), events.clone())
            .create(djinn_db::CreateSessionParams {
                project_id: &eval.id,
                task_id: Some(&task.id),
                model: "offline/injected-fixture",
                agent_type: "worker",
                metadata_json: Some(&metadata),
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .map_err(|error| error.to_string())?;
        let messages = fixture
            .messages
            .iter()
            .map(|message| {
                let role = match message.role.as_str() {
                    "system" => djinn_core::message::Role::System,
                    "user" => djinn_core::message::Role::User,
                    "assistant" => djinn_core::message::Role::Assistant,
                    role => {
                        return Err(format!(
                            "{} has unsupported persisted role {role}",
                            fixture.id
                        ));
                    }
                };
                Ok(djinn_core::message::Message {
                    role,
                    content: vec![djinn_core::message::ContentBlock::text(&message.content)],
                    metadata: None,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        SessionMessageRepository::new(db.clone(), events.clone())
            .insert_messages_batch(&session.id, &task.id, &messages)
            .await
            .map_err(|error| error.to_string())?;
        cases.push(DatabaseReplayCase {
            candidate_lookup_text: fixture.required_discriminative_text.clone(),
            fixture,
            session_id: session.id,
        });
    }
    let extractions = cases
        .iter()
        .map(|case| case.fixture.injected_provider_response.clone())
        .collect();
    let novelty = cases
        .iter()
        .filter(|case| {
            case.fixture.expect_adr_054_quality
                && (case.fixture.expected_duplicate_target.is_some()
                    || case.fixture.must_not_duplicate_target.is_some())
        })
        .map(
            |case| match case.fixture.expected_duplicate_target.as_deref() {
                Some(target) => {
                    format!(r#"{{"decision":"already_known","existing_note_id":"{target}"}}"#)
                }
                None => r#"{"decision":"novel","existing_note_id":null}"#.to_string(),
            },
        )
        .collect();
    let seam = ProductionReplaySeam {
        extraction_provider: Arc::new(FixtureResponseProvider::new(extractions)),
        novelty_provider: Arc::new(FixtureResponseProvider::new(novelty)),
    };
    let mut report = run_database_extraction_replay(db, events, &eval.id, &cases, &seam).await;
    normalize_offline_duplicate_targets(&mut report, &stable_target_by_database_id);
    Ok(report)
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
#[path = "extraction_replay_eval_tests.rs"]
mod tests;
