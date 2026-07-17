use std::sync::{Arc, Mutex};

use djinn_core::{events::EventBus, message::ContentBlock};
use djinn_db::{CreateSessionParams, ProjectRepository, SessionRepository, TaskRepository};
use djinn_provider::message::Conversation;
use djinn_provider::provider::{StreamEvent, ToolChoice};
use futures::{Future, Stream};
use std::collections::VecDeque;
use std::pin::Pin;

use super::*;

fn fixture(id: &str) -> ExtractionReplayFixture {
    ExtractionReplayFixture {
        id: id.to_string(),
        required_discriminative_text: "stable candidate seam".to_string(),
        expected_note_type: "case".to_string(),
        expect_adr_054_quality: false,
        must_not_duplicate_target: None,
        expected_duplicate_target: None,
        provenance: String::new(),
        messages: Vec::new(),
        terminal_context: crate::llm_extraction::TerminalExtractionContext::default(),
        injected_provider_response: String::new(),
    }
}

#[test]
fn zero_predicted_duplicates_has_zero_precision() {
    assert_eq!(DedupConfusionCounts::default().precision(), 0.0);
}

#[test]
fn committed_sanitized_corpus_is_valid() {
    let fixtures = load_extraction_replay_fixtures(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/extraction_replay"
    ))
    .expect("committed replay corpus must validate");
    assert_eq!(fixtures.len(), 24);
    assert!(
        fixtures
            .iter()
            .any(|fixture| fixture.expect_adr_054_quality)
    );
    assert!(
        fixtures
            .iter()
            .any(|fixture| !fixture.expect_adr_054_quality)
    );
    assert!(
        fixtures
            .iter()
            .any(|fixture| fixture.expected_duplicate_target.is_some())
    );
    assert!(
        fixtures
            .iter()
            .any(|fixture| fixture.must_not_duplicate_target.is_some())
    );
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
fn offline_report_serialization_normalizes_disposable_duplicate_ids() {
    let report_for = |database_id: &str| {
        let mut fixture = fixture("duplicate-case");
        fixture.expected_duplicate_target = Some(database_id.to_string());
        let observation = ExtractionObservation {
            fixture_id: "duplicate-case".to_string(),
            note_type: "case".to_string(),
            title: "candidate".to_string(),
            content: "stable candidate seam".to_string(),
            adr_054_quality_passed: false,
            duplicate_of: Some(database_id.to_string()),
        revision_operations: Vec::new(),
        };
        let mut report = score_extraction_replay(&[fixture], &[observation]);
        normalize_offline_duplicate_targets(
            &mut report,
            &BTreeMap::from([(database_id.to_string(), "candidate-duplicate".to_string())]),
        );
        report
    };

    let first = report_for("019f0000-0000-7000-8000-000000000001");
    let second = report_for("019f0000-0000-7000-8000-000000000002");
    assert_eq!(
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&second).unwrap()
    );
    assert_eq!(
        first.cases[0].observations[0].duplicate_of.as_deref(),
        Some("candidate-duplicate")
    );
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
    revision_operations: Vec::new(),
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
    revision_operations: Vec::new(),
        };
    let report = score_extraction_replay(&[fixture], &[observation]);
    assert_eq!(report.dedup.true_positive, 1);
    assert_eq!(report.dedup_precision, 1.0);
}

/// Deterministic LLM provider that returns pre-scripted responses in order.
/// Each `stream()` call pops one response from the front of its queue.
struct ScriptedProvider {
    responses: Mutex<VecDeque<String>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<String>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
        }
    }
}

impl djinn_provider::provider::LlmProvider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }

    fn stream<'a>(
        &'a self,
        _conversation: &'a Conversation,
        _tools: &'a [serde_json::Value],
        _tool_choice: Option<ToolChoice>,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = anyhow::Result<
                        Pin<Box<dyn Stream<Item = anyhow::Result<StreamEvent>> + Send>>,
                    >,
                > + Send
                + 'a,
        >,
    > {
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default();
        let stream = futures::stream::iter(vec![
            Ok(StreamEvent::Delta(ContentBlock::text(response))),
            Ok(StreamEvent::Done),
        ]);
        Box::pin(async move { Ok(Box::pin(stream) as _) })
    }
}

/// Minimal deterministic provider that always fails — used for the
/// provider-failure fixture-report test.
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
        revision_operations: Vec::new(),
        }])
    }
}

/// ADR-054-compliant case note body with all required sections in order.
/// This passes the production `assess_note_quality` gate so the novelty
/// decision path is exercised (capture only checks novelty when quality
/// passes).
const DURABLE_CASE_CONTENT: &str = "## Situation\n\
        A session-extracted case note is produced during task finalization.\n\n\
        ## Constraint\n\
        The existing candidate must be a low-confidence, session-extracted note with a provenance footer.\n\n\
        ## Approach taken\n\
        Merge the fresh evidence into the existing note body and keep the provenance footer.\n\n\
        ## Result\n\
        The merged body contains both the original evidence and the new evidence, with the footer preserved.\n\n\
        ## Why it worked / failed\n\
        The merge preserves provenance and avoids wholesale replacement while allowing future sessions to contribute additional evidence.\n\n\
        ## Reusable lesson\n\
        Session-extracted notes below the curation threshold can absorb new evidence from later sessions without losing their original provenance.\n\n\
        ## Related\n\
        - extraction merge\n\
        - provenance";

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

/// Persist the complete committed fixture transcript and retain its typed
/// terminal context in session metadata for the database-backed replay.
async fn create_fixture_replay_session(
    db: &Database,
    events: EventBus,
    project_id: &str,
    fixture: &ExtractionReplayFixture,
) -> String {
    let task = TaskRepository::new(db.clone(), events.clone())
        .create_in_project(
            project_id,
            None,
            &format!("replay {}", fixture.id),
            "committed replay fixture task",
            "",
            "task",
            1,
            "eval",
            None,
            None,
        )
        .await
        .unwrap();
    let terminal_context = serde_json::to_string(&fixture.terminal_context).unwrap();
    let session = SessionRepository::new(db.clone(), events.clone())
        .create(CreateSessionParams {
            project_id,
            task_id: Some(&task.id),
            model: "deterministic/replay",
            agent_type: "worker",
            metadata_json: Some(&terminal_context),
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
    let messages = fixture
        .messages
        .iter()
        .map(|message| djinn_core::message::Message {
            role: match message.role.as_str() {
                "system" => djinn_core::message::Role::System,
                "user" => djinn_core::message::Role::User,
                "assistant" => djinn_core::message::Role::Assistant,
                unsupported => panic!("unsupported persisted fixture role: {unsupported}"),
            },
            content: vec![ContentBlock::text(&message.content)],
            metadata: None,
        })
        .collect::<Vec<_>>();
    SessionMessageRepository::new(db.clone(), events)
        .insert_messages_batch(&session.id, &task.id, &messages)
        .await
        .unwrap();
    session.id
}

#[tokio::test]
async fn database_replay_drives_production_seam_with_dedup_and_non_dedup_outcomes() {
    let db = Database::open_in_memory().unwrap();
    let events = EventBus::noop();
    let projects = ProjectRepository::new(db.clone(), events.clone());
    let live = projects.create("live", "test", "live").await.unwrap();
    let eval = projects.create("eval", "test", "eval").await.unwrap();
    let notes = NoteRepository::new(db.clone(), events.clone());

    // Sentinel in the live corpus — must be byte-for-byte unchanged after eval.
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

    // Candidate seeded through NoteRepository in the eval project. The
    // production candidate lookup (dedup_candidates) must return it so the
    // novelty decision path has a real target to select or reject.
    let duplicate_candidate = notes
        .create_db_note_with_permalink(
            &eval.id,
            "dup-candidate",
            "Duplicate Candidate",
            DURABLE_CASE_CONTENT,
            "case",
            "[]",
        )
        .await
        .unwrap();

    // A second candidate that the provider will NOT select — proving the
    // must-not-duplicate outcome. It is seeded in the same eval project so
    // the production lookup returns it alongside the duplicate candidate.
    let nondup_candidate = notes
        .create_db_note_with_permalink(
            &eval.id,
            "nondup-candidate",
            "Non-Duplicate Candidate",
            DURABLE_CASE_CONTENT,
            "case",
            "[]",
        )
        .await
        .unwrap();

    let before_live = serde_json::to_vec(&notes.list(&live.id, None).await.unwrap()).unwrap();
    let eval_note_count_before = notes.count_by_project(&eval.id).await.unwrap();

    // Create two sessions: one for the dedup case, one for the non-dedup case.
    let dedup_session =
        create_replay_session(&db, events.clone(), &eval.id, "dedup fixture transcript").await;
    let nondup_session = create_replay_session(
        &db,
        events.clone(),
        &eval.id,
        "non-dedup fixture transcript",
    )
    .await;

    // Extraction response JSON — a single ADR-054-compliant case note. The
    // same extraction response is used for both cases; the difference is
    // only in the novelty decision.
    let extraction_json = serde_json::json!({
        "cases": [{
            "title": "Fresh Case",
            "content": DURABLE_CASE_CONTENT,
            "applies_when": "When merging fresh evidence into an existing note."
        }],
        "patterns": [],
        "pitfalls": [],
    })
    .to_string();

    // Fixture A: expects the extracted note to duplicate duplicate_candidate
    // (positive dedup — provider says "already_known").
    let dedup_fixture = ExtractionReplayFixture {
        id: "dedup-case".to_string(),
        required_discriminative_text: "extraction merge".to_string(),
        expected_note_type: "case".to_string(),
        expect_adr_054_quality: true,
        must_not_duplicate_target: None,
        expected_duplicate_target: Some(duplicate_candidate.id.clone()),
        provenance: "sanitized_archived_transcript_derived".to_string(),
        messages: vec![ReplayTranscriptMessage {
            role: "user".to_string(),
            content: "dedup fixture transcript".to_string(),
        }],
        terminal_context: crate::llm_extraction::TerminalExtractionContext::default(),
        injected_provider_response: extraction_json.clone(),
    };

    // Fixture B: expects the extracted note to be novel relative to all
    // supplied candidates (must-not-duplicate — provider says "novel").
    let nondup_fixture = ExtractionReplayFixture {
        id: "nondup-case".to_string(),
        required_discriminative_text: "extraction merge".to_string(),
        expected_note_type: "case".to_string(),
        expect_adr_054_quality: true,
        must_not_duplicate_target: Some(nondup_candidate.id.clone()),
        expected_duplicate_target: None,
        provenance: "sanitized_archived_transcript_derived".to_string(),
        messages: vec![ReplayTranscriptMessage {
            role: "user".to_string(),
            content: "non-dedup fixture transcript".to_string(),
        }],
        terminal_context: crate::llm_extraction::TerminalExtractionContext::default(),
        injected_provider_response: extraction_json.clone(),
    };

    // The novelty provider for the dedup case returns "already_known"
    // pointing at duplicate_candidate. For the non-dedup case it returns
    // "novel". Since each case drives one extraction call + one novelty
    // call, the provider needs 2 extraction responses and 2 novelty
    // responses in order.
    let provider = Arc::new(ScriptedProvider::new(vec![
        // Case 1 (dedup): extraction response
        extraction_json.clone(),
        // Case 1 (dedup): novelty decision — already_known
        format!(
            r#"{{"decision":"already_known","existing_note_id":"{}"}}"#,
            duplicate_candidate.id
        ),
        // Case 2 (nondup): extraction response
        extraction_json,
        // Case 2 (nondup): novelty decision — novel
        r#"{"decision":"novel","existing_note_id":null}"#.to_string(),
    ]));

    let seam = ProductionReplaySeam {
        extraction_provider: provider.clone(),
        novelty_provider: provider,
    };

    let report = run_database_extraction_replay(
        db.clone(),
        events,
        &eval.id,
        &[
            DatabaseReplayCase {
                fixture: dedup_fixture,
                session_id: dedup_session,
                candidate_lookup_text: DURABLE_CASE_CONTENT.to_string(),
            },
            DatabaseReplayCase {
                fixture: nondup_fixture,
                session_id: nondup_session,
                candidate_lookup_text: DURABLE_CASE_CONTENT.to_string(),
            },
        ],
        &seam,
    )
    .await;

    // ── Per-case assertions ────────────────────────────────────────────
    assert_eq!(report.total_cases, 2);

    let dedup_result = report
        .cases
        .iter()
        .find(|c| c.fixture_id == "dedup-case")
        .expect("dedup-case result present");
    assert!(
        dedup_result.observations[0].adr_054_quality_passed,
        "production capture must have passed ADR-054 gate"
    );
    assert_eq!(
        dedup_result.observations[0].duplicate_of.as_deref(),
        Some(duplicate_candidate.id.as_str()),
        "production novelty must have selected the seeded duplicate candidate"
    );
    assert!(dedup_result.rubric.must_not_duplicate);

    let nondup_result = report
        .cases
        .iter()
        .find(|c| c.fixture_id == "nondup-case")
        .expect("nondup-case result present");
    assert!(
        nondup_result.observations[0].adr_054_quality_passed,
        "production capture must have passed ADR-054 gate"
    );
    assert!(
        nondup_result.observations[0].duplicate_of.is_none(),
        "production novelty must not have selected any candidate"
    );
    assert!(nondup_result.rubric.must_not_duplicate);

    // ── Aggregate dedup assertions ─────────────────────────────────────
    // dedup-case: predicted=Some(candidate), expected=Some(candidate) → TP
    // nondup-case: predicted=None, expected=None → TN
    assert_eq!(report.dedup.true_positive, 1);
    assert_eq!(report.dedup.true_negative, 1);
    assert_eq!(report.dedup.false_positive, 0);
    assert_eq!(report.dedup.false_negative, 0);
    assert_eq!(report.dedup_precision, 1.0);

    // ── Live-corpus isolation ──────────────────────────────────────────
    let after_notes = notes.list(&live.id, None).await.unwrap();
    let after_live = serde_json::to_vec(&after_notes).unwrap();
    assert_eq!(
        before_live, after_live,
        "live corpus must be byte-for-byte unchanged"
    );
    assert_eq!(after_notes.len(), 1);
    assert_eq!(after_notes[0].id, sentinel.id);
    assert_eq!(
        notes.count_by_project(&eval.id).await.unwrap(),
        eval_note_count_before,
        "replay must not persist generated notes"
    );
}

#[tokio::test]
async fn committed_corpus_replays_every_fixture_through_the_database_runner() {
    let fixtures = load_extraction_replay_fixtures(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/extraction_replay"
    ))
    .expect("committed replay corpus must validate");
    let db = Database::open_in_memory().unwrap();
    let events = EventBus::noop();
    let eval = ProjectRepository::new(db.clone(), events.clone())
        .create("eval", "test", "committed-corpus")
        .await
        .unwrap();
    let notes = NoteRepository::new(db.clone(), events.clone());
    let mut replay_cases = Vec::with_capacity(fixtures.len());
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
                .unwrap();
            *target = candidate.id;
        }
        let session_id =
            create_fixture_replay_session(&db, events.clone(), &eval.id, &fixture).await;
        replay_cases.push(DatabaseReplayCase {
            candidate_lookup_text: fixture.required_discriminative_text.clone(),
            fixture,
            session_id,
        });
    }

    let extraction_responses = replay_cases
        .iter()
        .map(|case| case.fixture.injected_provider_response.clone())
        .collect();
    let novelty_responses = replay_cases
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
        extraction_provider: Arc::new(ScriptedProvider::new(extraction_responses)),
        novelty_provider: Arc::new(ScriptedProvider::new(novelty_responses)),
    };
    let report = run_database_extraction_replay(db, events, &eval.id, &replay_cases, &seam).await;

    assert_eq!(report.total_cases, 24);
    assert_eq!(report.cases.len(), replay_cases.len());
    assert_eq!(report.satisfied_cases, report.total_cases);
    assert_eq!(report.rubric_satisfaction_rate, 1.0);
    assert_eq!(report.dedup.false_positive, 0);
    assert_eq!(report.dedup.false_negative, 0);
    assert_eq!(report.dedup_precision, 1.0);
    assert!(report.failures.is_empty(), "{:#?}", report.failures);
}

#[tokio::test]
async fn provider_failure_stays_in_fixture_report() {
    let db = Database::open_in_memory().unwrap();
    let events = EventBus::noop();
    let eval = ProjectRepository::new(db.clone(), events.clone())
        .create("eval", "test", "provider-failure")
        .await
        .unwrap();
    let session_id = create_replay_session(&db, events.clone(), &eval.id, "failure fixture").await;
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
