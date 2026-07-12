#![allow(clippy::too_many_lines)]
// djinn:allow-oversize — legacy test module over size-guard threshold; split when touched substantively.
// Tests for the full reflection pipeline and extracted-note persistence.
//!
//! Covers:
//! AC1 - Session completion triggers full reflection pipeline
//! AC2 - Structural extraction produces co-access pairs and event taxonomy
//! AC3 - LLM extraction with FakeProvider produces case/pattern/pitfall notes
//! AC4 - Extracted notes have confidence 0.5 and session provenance in content
//! AC5 - Graceful degradation: LLM unavailable → no notes written, no errors
//! AC6 - Dedup pipeline: repeated sessions do not create duplicate notes

use std::sync::{Arc, Mutex};

use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use djinn_core::message::{ContentBlock, Message, Role};
use djinn_db::{
    CreateSessionParams, EpicCreateInput, EpicRepository, NoteConsolidationRepository,
    NoteDedupCandidate, NoteRepository, ProjectRepository, SessionRepository, TaskRepository,
};

use crate::llm_extraction::{
    run_llm_extraction, run_llm_extraction_with_provider,
    run_llm_extraction_with_provider_and_candidate_lookup,
};
use crate::session_extraction::{SessionTaxonomy, extract_session_signals};
use crate::test_helpers::{FailingProvider, FakeProvider, agent_context_from_db, create_test_db};

/// Creates a temp directory (notes will be written there).
fn make_tmpdir() -> TempDir {
    crate::test_helpers::test_tempdir("djinn-llm-extraction-")
}

#[derive(Clone, Default)]
struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

impl CapturedLogs {
    fn take(&self) -> String {
        let mut buf = self.0.lock().expect("captured logs mutex poisoned");
        let out = String::from_utf8(buf.clone()).expect("captured log bytes were valid utf-8");
        buf.clear();
        out
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
    type Writer = CapturedLogsWriter;
    fn make_writer(&'a self) -> Self::Writer {
        CapturedLogsWriter {
            inner: Arc::clone(&self.0),
        }
    }
}

struct CapturedLogsWriter {
    inner: Arc<Mutex<Vec<u8>>>,
}

impl std::io::Write for CapturedLogsWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner
            .lock()
            .expect("captured logs mutex poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// Task #8: the `llm_extraction_routes_durable_writes_into_task_worktree_when_session_has_one`
// test covered the `sessions.worktree_path` migration-window fallback that
// routed LLM-extracted notes into the old per-task worktree.  That fallback
// has been removed — workspace_path now only comes from `task_runs`, and the
// per-task worktree directory is no longer created.  Task #13 will drop the
// column outright.

static SEMANTIC_DUPLICATE_CANDIDATE_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn semantic_duplicate_candidate_lookup(
    _project_id: &str,
    _folder: &str,
    _note_type: &str,
    _candidate_abstract: &str,
) -> Vec<NoteDedupCandidate> {
    let existing_id = SEMANTIC_DUPLICATE_CANDIDATE_ID
        .get()
        .expect("semantic duplicate candidate id configured");
    vec![novelty_candidate(existing_id)]
}

struct TestFixture {
    db: djinn_db::Database,
    cancel: CancellationToken,
    project: djinn_core::models::Project,
    task: djinn_core::models::Task,
    session_id: String,
    tmpdir: TempDir,
}

impl TestFixture {
    fn note_repo(&self) -> NoteRepository {
        NoteRepository::new(self.db.clone(), djinn_core::events::EventBus::noop())
    }
}

/// Build a complete test fixture: DB + project + epic + task + session.
async fn make_fixture() -> TestFixture {
    let tmpdir = make_tmpdir();
    let db = create_test_db();
    let cancel = CancellationToken::new();
    let events = djinn_core::events::EventBus::noop();
    let project_repo = ProjectRepository::new(db.clone(), events.clone());
    let epic_repo = EpicRepository::new(db.clone(), events.clone());
    let task_repo = TaskRepository::new(db.clone(), events.clone());
    let session_repo = SessionRepository::new(db.clone(), events.clone());
    let uid = uuid::Uuid::now_v7().to_string();
    let name = format!("test-project-{uid}");
    let project = project_repo
        .create(&name, "test", &name)
        .await
        .expect("create project");
    let epic = epic_repo
        .create_for_project(
            &project.id,
            EpicCreateInput {
                title: "test-epic",
                description: "desc",
                emoji: "🧪",
                color: "blue",
                owner: "test",
                memory_refs: None,
                status: None,
                auto_breakdown: None,
                originating_adr_id: None,
                blocked_by: None,
            },
        )
        .await
        .expect("create epic");
    let task = task_repo
        .create_in_project(
            &project.id,
            Some(&epic.id),
            "test-task",
            "implement the test feature",
            "test design",
            "task",
            2,
            "test",
            None,
            None,
        )
        .await
        .expect("create task");
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &project.id,
            task_id: Some(&task.id),
            model: "test-model",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("create session");
    TestFixture {
        db,
        cancel,
        project,
        task,
        session_id: session.id,
        tmpdir,
    }
}

/// Build a FakeProvider that returns a valid extraction JSON with one of each note type.
fn fake_extraction_provider() -> Arc<FakeProvider> {
    let json = serde_json::json!({
        "cases": [{
            "title": "Test Case Note",
            "content": "## Situation\nA flaky extraction pipeline had to compare candidate summaries under a deterministic constraint.\n## Constraint\nNovelty checks needed stable inputs across repeated runs and future tasks.\n## Approach taken\nInject a stable candidate seam and keep the comparison summary explicit in the extraction flow.\n## Result\nThe extraction remained deterministic and avoided duplicate durable notes.\n## Why it worked / failed\nThe seam removed unstable inputs that were previously changing across runs.\n## Reusable lesson\nUse an explicit deterministic seam when extraction quality depends on comparing generated summaries reliably.\n## Related\n- novelty detection\n- extraction quality gates"
        }],
        "patterns": [{
            "title": "Test Pattern Note",
            "content": "## Context\nA workflow needs deterministic comparisons while still preserving reusable extraction behavior.\n## Problem shape\nUnstable provider responses can create noisy differences that look novel even when the underlying knowledge is the same.\n## Recommended approach\nIntroduce a reusable seam for the comparison inputs and keep the durable evaluation steps explicit.\n## Why it works\nThe seam isolates unstable dependencies and preserves a repeatable decision path.\n## Tradeoffs / limits\nIt adds test scaffolding and only helps when the comparison boundary is well understood.\n## When to use\nUse this when future tasks must compare summaries or candidates deterministically across repeated runs.\n## When not to use\nDo not use it when the workflow is intentionally exploratory or the comparison boundary is still changing.\n## Related\n- novelty detection\n- deterministic tests"
        }],
        "pitfalls": [{
            "title": "Test Pitfall Note",
            "content": "## Trigger / smell\nSemantic duplicate checks become flaky when summaries change between runs.\n## Failure mode\nExtraction creates noisy sibling notes instead of recognizing the same durable knowledge.\n## Observable symptoms\nRepeated runs alternate between merging and writing new notes with nearly identical content.\n## Prevention\nInject stable summaries and keep the comparison contract narrow and explicit.\n## Recovery\nReplace unstable inputs with deterministic fixtures and rerun the novelty gate.\n## Related\n- duplicate notes\n- extraction quality gates"
        }]
    })
    .to_string();
    Arc::new(FakeProvider::text(&json))
}

fn novelty_candidate(existing_id: &str) -> NoteDedupCandidate {
    NoteDedupCandidate {
        id: existing_id.to_string(),
        permalink: "cases/existing-semantic-note".to_string(),
        title: "Existing Semantic Note".to_string(),
        folder: "cases".to_string(),
        note_type: "case".to_string(),
        content: "Full existing note body for novelty testing.".to_string(),
        abstract_: Some(
            "Fix flaky semantic duplicate tests by injecting dedup candidates.".to_string(),
        ),
        overview: Some(
            "Inject a stable candidate seam so novelty compares summaries deterministically."
                .to_string(),
        ),
        score: 1.0,
    }
}

fn novelty_failure_candidate_lookup(
    _project_id: &str,
    _folder: &str,
    _note_type: &str,
    _candidate_abstract: &str,
) -> Vec<NoteDedupCandidate> {
    vec![NoteDedupCandidate {
        id: "candidate-for-invalid-json".to_string(),
        permalink: "cases/candidate-for-invalid-json".to_string(),
        title: "Candidate For Invalid JSON".to_string(),
        folder: "cases".to_string(),
        note_type: "case".to_string(),
        content: "Full existing candidate body.".to_string(),
        abstract_: Some("Existing candidate summary".to_string()),
        overview: Some("Existing candidate overview".to_string()),
        score: 1.0,
    }]
}

#[test]
fn structural_extraction_produces_correct_taxonomy() {
    let messages = vec![
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "memory_read".into(),
                input: serde_json::json!({"identifier": "decisions/adr-001", "project": "/tmp"}),
            }],
            metadata: None,
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t2".into(),
                name: "memory_read".into(),
                input: serde_json::json!({"identifier": "decisions/adr-002", "project": "/tmp"}),
            }],
            metadata: None,
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t3".into(),
                name: "write".into(),
                input: serde_json::json!({"path": "src/main.rs", "content": "fn main() {}"}),
            }],
            metadata: None,
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "t3".into(),
                content: vec![ContentBlock::text("error: permission denied")],
                is_error: true,
            }],
            metadata: None,
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t4".into(),
                name: "task_transition".into(),
                input: serde_json::json!({"task_id": "abc", "action": "done"}),
            }],
            metadata: None,
        },
    ];
    let signals = extract_session_signals(&messages);
    assert_eq!(signals.taxonomy.notes_read, 2);
    assert_eq!(signals.taxonomy.errors, 1);
    assert_eq!(signals.taxonomy.files_changed, 1);
    assert_eq!(signals.taxonomy.tasks_transitioned, 1);
    assert_eq!(signals.taxonomy.tools_used, 3);
    assert_eq!(signals.notes_read_ids.len(), 2);
    assert!(
        signals
            .notes_read_ids
            .contains(&"decisions/adr-001".to_string())
    );
    assert!(
        signals
            .notes_read_ids
            .contains(&"decisions/adr-002".to_string())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn structural_extraction_flushes_co_access_associations() {
    let fixture = make_fixture().await;
    let ctx = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());
    let note_repo = fixture.note_repo();
    let note_a = note_repo
        .create(
            &fixture.project.id,
            "Note Alpha",
            "content alpha",
            "reference",
            "[]",
        )
        .await
        .expect("create note_a");
    let note_b = note_repo
        .create(
            &fixture.project.id,
            "Note Beta",
            "content beta",
            "reference",
            "[]",
        )
        .await
        .expect("create note_b");
    let messages = vec![
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "memory_read".into(),
                input: serde_json::json!({
                    "identifier": note_a.permalink,
                    "project": fixture.project.slug()
                }),
            }],
            metadata: None,
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t2".into(),
                name: "memory_read".into(),
                input: serde_json::json!({
                    "identifier": note_b.permalink,
                    "project": fixture.project.slug()
                }),
            }],
            metadata: None,
        },
    ];
    let taxonomy = crate::session_extraction::run_structural_extraction(
        fixture.session_id.clone(),
        messages,
        ctx,
    )
    .await;
    assert!(taxonomy.is_some());
    let taxonomy = taxonomy.expect("taxonomy present");
    assert_eq!(taxonomy.notes_read, 2);
    let associations = note_repo
        .get_associations_for_note(&note_a.id)
        .await
        .expect("get associations");
    assert!(!associations.is_empty());
    let assoc = &associations[0];
    let other_id = if assoc.note_a_id == note_a.id {
        &assoc.note_b_id
    } else {
        &assoc.note_a_id
    };
    assert_eq!(other_id, &note_b.id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_extraction_with_fake_provider_writes_case_pattern_pitfall_notes() {
    let fixture = make_fixture().await;
    let ctx = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());
    let taxonomy = SessionTaxonomy {
        files_changed: 3,
        errors: 2,
        tools_used: 6,
        notes_read: 1,
        notes_written: 2,
        tasks_transitioned: 1,
        ..SessionTaxonomy::default()
    };
    let provider = fake_extraction_provider();
    run_llm_extraction_with_provider(fixture.session_id.clone(), taxonomy, ctx, provider).await;
    let note_repo = fixture.note_repo();
    let all_notes = note_repo
        .list(&fixture.project.id, None)
        .await
        .expect("list notes");
    let cases: Vec<_> = all_notes.iter().filter(|n| n.note_type == "case").collect();
    let patterns: Vec<_> = all_notes
        .iter()
        .filter(|n| n.note_type == "pattern")
        .collect();
    let pitfalls: Vec<_> = all_notes
        .iter()
        .filter(|n| n.note_type == "pitfall")
        .collect();
    assert_eq!(cases.len(), 1);
    assert_eq!(patterns.len(), 1);
    assert_eq!(pitfalls.len(), 1);
    assert_eq!(cases[0].title, "Test Case Note");
    assert_eq!(patterns[0].title, "Test Pattern Note");
    assert_eq!(pitfalls[0].title, "Test Pitfall Note");
    for note in [cases[0], patterns[0], pitfalls[0]] {
        assert_eq!(note.storage, "db");
        assert!(note.file_path.is_empty());
    }
    assert!(
        !fixture
            .tmpdir
            .path()
            .join(".djinn/cases/test-case-note.md")
            .exists()
    );
    assert!(
        !fixture
            .tmpdir
            .path()
            .join(".djinn/patterns/test-pattern-note.md")
            .exists()
    );
    assert!(
        !fixture
            .tmpdir
            .path()
            .join(".djinn/pitfalls/test-pitfall-note.md")
            .exists()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_extracted_notes_have_confidence_0_5() {
    let fixture = make_fixture().await;
    let ctx = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());
    let taxonomy = SessionTaxonomy {
        files_changed: 2,
        errors: 1,
        tools_used: 4,
        notes_read: 0,
        notes_written: 1,
        tasks_transitioned: 1,
        ..SessionTaxonomy::default()
    };
    let provider = fake_extraction_provider();
    run_llm_extraction_with_provider(fixture.session_id.clone(), taxonomy, ctx, provider).await;
    let note_repo = fixture.note_repo();
    let all_notes = note_repo
        .list(&fixture.project.id, None)
        .await
        .expect("list notes");
    assert!(!all_notes.is_empty());
    for note in &all_notes {
        assert!((note.confidence - 0.5).abs() < 1e-9);
    }
    let stored_json =
        SessionRepository::new(fixture.db.clone(), djinn_core::events::EventBus::noop())
            .get_event_taxonomy_json(&fixture.session_id)
            .await
            .expect("query session event_taxonomy after llm extraction");
    let stored_taxonomy: SessionTaxonomy = serde_json::from_str(stored_json.as_deref().unwrap())
        .expect("deserialize stored taxonomy after llm extraction");
    assert_eq!(stored_taxonomy.extraction_quality.extracted, 3);
    assert_eq!(stored_taxonomy.extraction_quality.dedup_skipped, 0);
    assert_eq!(stored_taxonomy.extraction_quality.novelty_skipped, 0);
    assert_eq!(stored_taxonomy.extraction_quality.written, 3);
    assert_eq!(stored_taxonomy.extraction_quality.merged, 0);
    assert_eq!(stored_taxonomy.extraction_quality.downgraded, 0);
    assert_eq!(stored_taxonomy.extraction_quality.discarded, 0);
    assert_eq!(stored_taxonomy.extraction_quality.admission_dropped, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_extracted_notes_contain_session_id_provenance() {
    let fixture = make_fixture().await;
    let session_id = fixture.session_id.clone();
    let ctx = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());
    let taxonomy = SessionTaxonomy::default();
    let json = serde_json::json!({
        "cases": [{
            "title": "Provenance Test",
            "content": "## Situation\nExtraction provenance must remain visible after a durable note is written.\n## Constraint\nFuture tasks need to know which session produced the case while keeping the note reusable.\n## Approach taken\nAppend a provenance footer and preserve the worked example in the durable case body.\n## Result\nThe stored case stays traceable without losing its reusable content.\n## Why it worked / failed\nThe footer keeps session origin explicit while leaving the durable lesson intact.\n## Reusable lesson\nKeep provenance appended separately so future tasks can trust the origin of extracted durable notes.\n## Related\n- provenance\n- durable extraction"
        }],
        "patterns": [],
        "pitfalls": []
    })
    .to_string();
    let provider = Arc::new(FakeProvider::text(&json));
    run_llm_extraction_with_provider(session_id.clone(), taxonomy, ctx, provider).await;
    let note_repo = fixture.note_repo();
    let notes = note_repo
        .list(&fixture.project.id, None)
        .await
        .expect("list notes");
    assert_eq!(notes.len(), 1);
    let note = &notes[0];
    assert!(note.content.contains(&session_id));
    assert!(note.content.contains("Extracted from session"));
    assert!(note.content.contains("0.5"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_extraction_graceful_degradation_failing_provider_no_notes_written() {
    let fixture = make_fixture().await;
    let ctx = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());
    let taxonomy = SessionTaxonomy {
        files_changed: 5,
        errors: 3,
        tools_used: 8,
        notes_read: 1,
        notes_written: 2,
        tasks_transitioned: 1,
        ..SessionTaxonomy::default()
    };
    let provider = Arc::new(FailingProvider::new("injected LLM failure for test"));
    run_llm_extraction_with_provider(fixture.session_id.clone(), taxonomy, ctx, provider).await;
    let note_repo = fixture.note_repo();
    let notes = note_repo
        .list(&fixture.project.id, None)
        .await
        .expect("list notes");
    assert!(notes.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_extraction_distinguishes_empty_success_from_failed_call() {
    // EMPTY-but-successful: the LLM returned valid JSON with empty arrays. The
    // extraction is persisted with `extracted: 0` (normal, nothing to record).
    let empty_fixture = make_fixture().await;
    let empty_ctx = agent_context_from_db(empty_fixture.db.clone(), empty_fixture.cancel.clone());
    let empty_provider = Arc::new(FakeProvider::text(
        r#"{"cases":[],"patterns":[],"pitfalls":[]}"#,
    ));
    run_llm_extraction_with_provider(
        empty_fixture.session_id.clone(),
        SessionTaxonomy::default(),
        empty_ctx,
        empty_provider,
    )
    .await;
    let empty_note_repo = empty_fixture.note_repo();
    assert!(
        empty_note_repo
            .list(&empty_fixture.project.id, None)
            .await
            .expect("list notes")
            .is_empty(),
        "empty extraction writes no notes"
    );
    let empty_taxonomy_json = SessionRepository::new(
        empty_fixture.db.clone(),
        djinn_core::events::EventBus::noop(),
    )
    .get_event_taxonomy_json(&empty_fixture.session_id)
    .await
    .expect("query event_taxonomy after empty success");
    // Empty-success PERSISTS the taxonomy (extracted = 0) — it is a recorded
    // outcome, not an error.
    let empty_taxonomy: SessionTaxonomy =
        serde_json::from_str(empty_taxonomy_json.as_deref().expect("taxonomy persisted"))
            .expect("deserialize taxonomy after empty success");
    assert_eq!(empty_taxonomy.extraction_quality.extracted, 0);
    // FAILED: the LLM call itself errored. Nothing is persisted — the failure
    // path returns before persisting the extraction taxonomy.
    let failed_fixture = make_fixture().await;
    let failed_ctx =
        agent_context_from_db(failed_fixture.db.clone(), failed_fixture.cancel.clone());
    let failed_provider = Arc::new(FailingProvider::new("injected extraction-call failure"));
    run_llm_extraction_with_provider(
        failed_fixture.session_id.clone(),
        SessionTaxonomy::default(),
        failed_ctx,
        failed_provider,
    )
    .await;
    let failed_note_repo = failed_fixture.note_repo();
    assert!(
        failed_note_repo
            .list(&failed_fixture.project.id, None)
            .await
            .expect("list notes")
            .is_empty(),
        "failed extraction writes no notes"
    );
    let failed_taxonomy_json = SessionRepository::new(
        failed_fixture.db.clone(),
        djinn_core::events::EventBus::noop(),
    )
    .get_event_taxonomy_json(&failed_fixture.session_id)
    .await
    .expect("query event_taxonomy after failed call");
    assert!(
        failed_taxonomy_json.is_none(),
        "failed extraction must NOT persist an extraction taxonomy — it is an error, \
         distinct from an empty-but-successful extraction"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_extraction_intra_batch_dedup_collapses_duplicate_notes() {
    // The model emits the same case twice (identical title modulo case) in one
    // extraction. Intra-batch dedup must collapse them to a single written note
    // and record the dropped duplicate in `dedup_skipped`.
    let fixture = make_fixture().await;
    let ctx = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());
    let dup_body = "## Situation\nA flaky extraction pipeline emitted the same case twice in one batch under a deterministic constraint.\n## Constraint\nThe durable note must be written once even when the model repeats it across the response.\n## Approach taken\nNormalize the title and note type and drop the second occurrence before any DB work happens.\n## Result\nOnly one durable case was created and the duplicate was counted as skipped.\n## Why it worked / failed\nThe normalized key collapsed the repeat without losing the reusable lesson.\n## Reusable lesson\nDeduplicate generated notes by a normalized title and type before creating them to avoid duplicate durable writes.\n## Related\n- intra-batch dedup\n- extraction quality gates";
    let json = serde_json::json!({
        "cases": [
            { "title": "Duplicate Case", "content": dup_body },
            { "title": "  duplicate case ", "content": dup_body }
        ],
        "patterns": [],
        "pitfalls": []
    })
    .to_string();
    let provider = Arc::new(FakeProvider::text(&json));
    run_llm_extraction_with_provider(
        fixture.session_id.clone(),
        SessionTaxonomy::default(),
        ctx,
        provider,
    )
    .await;
    let note_repo = fixture.note_repo();
    let cases: Vec<_> = note_repo
        .list(&fixture.project.id, None)
        .await
        .expect("list notes")
        .into_iter()
        .filter(|n| n.note_type == "case")
        .collect();
    assert_eq!(
        cases.len(),
        1,
        "two notes with the same normalized title+type collapse to one"
    );
    let stored_json =
        SessionRepository::new(fixture.db.clone(), djinn_core::events::EventBus::noop())
            .get_event_taxonomy_json(&fixture.session_id)
            .await
            .expect("query event_taxonomy after intra-batch dedup");
    let stored_taxonomy: SessionTaxonomy =
        serde_json::from_str(stored_json.as_deref().expect("taxonomy persisted"))
            .expect("deserialize taxonomy after intra-batch dedup");
    assert_eq!(
        stored_taxonomy.extraction_quality.extracted, 1,
        "only the unique note counts as extracted"
    );
    assert_eq!(
        stored_taxonomy.extraction_quality.dedup_skipped, 1,
        "the intra-batch duplicate is recorded as dedup_skipped"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_extraction_graceful_degradation_no_provider_configured() {
    let fixture = make_fixture().await;
    let ctx = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());
    let taxonomy = SessionTaxonomy {
        files_changed: 2,
        errors: 0,
        tools_used: 3,
        notes_read: 0,
        notes_written: 1,
        tasks_transitioned: 1,
        ..SessionTaxonomy::default()
    };
    let logs = CapturedLogs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(logs.clone())
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NONE)
        .with_target(false)
        .with_ansi(false)
        .with_level(true)
        .finish();
    let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
    let guard = tracing::dispatcher::set_default(&dispatch);
    run_llm_extraction(fixture.session_id.clone(), taxonomy, ctx).await;
    drop(guard);
    let captured = logs.take();
    assert!(
        captured.contains("llm_extraction: no LLM provider available; skipping extraction"),
        "missing-provider path should emit the provider-unavailable warning; captured: {captured}"
    );
    assert!(
        captured.contains("provider_resolution_stage"),
        "missing-provider path should include structured resolution-stage diagnostics; captured: {captured}"
    );
    assert!(
        !captured.contains("dropping underspecified note at admission gate"),
        "missing-provider path must stay distinct from admission/quality-gate rejection; captured: {captured}"
    );
    let note_repo = fixture.note_repo();
    let notes = note_repo
        .list(&fixture.project.id, None)
        .await
        .expect("list notes");
    assert!(notes.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_extraction_semantic_duplicate_skips_create_and_boosts_existing_confidence() {
    let fixture = make_fixture().await;
    let ctx = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());
    let note_repo = fixture.note_repo();
    let existing = note_repo
        .create_db_note(
            &fixture.project.id,
            "Existing Semantic Note",
            "Existing content",
            "case",
            "[]",
        )
        .await
        .expect("create existing note");
    note_repo
        .update_summaries(
            &existing.id,
            Some("Fix flaky semantic duplicate tests by injecting dedup candidates."),
            Some("Inject a stable candidate seam so novelty compares summaries deterministically."),
        )
        .await
        .expect("update summaries");
    note_repo
        .set_confidence(&existing.id, 0.5)
        .await
        .expect("set starting confidence");
    let starting_confidence = note_repo
        .get(&existing.id)
        .await
        .expect("get existing before run")
        .expect("existing note before run")
        .confidence;
    let taxonomy = SessionTaxonomy {
        files_changed: 2,
        errors: 1,
        tools_used: 4,
        notes_read: 0,
        notes_written: 1,
        tasks_transitioned: 1,
        ..SessionTaxonomy::default()
    };
    let provider = Arc::new(FakeProvider::script(vec![
        vec![
            djinn_provider::provider::StreamEvent::Delta(ContentBlock::Text {
                text: serde_json::json!({
                    "cases": [{
                        "title": "Duplicate Semantic Note",
                        "content": "## Situation\nA semantic duplicate test exercises the novelty path and needs a stable case body. The body must satisfy the ADR-054 admission gate so the duplicate path is reached.\n\n## Constraint\nThe novelty judge must be reached for a complete ADR-054 candidate; the existing seam depends on the candidate passing the gate first.\n\n## Approach taken\nInject a stable candidate seam and keep the comparison summary explicit in the extraction flow so the duplicate path is exercised.\n\n## Result\nThe case flows into the novelty judge and the duplicate path boosts the existing note's confidence as expected.\n\n## Why it worked / failed\nThe seam removes unstable inputs that would otherwise change the durable body across runs and cause the gate to drop the candidate.\n\n## Reusable lesson\nStable candidate seams are required to exercise the novelty/dedup path deterministically across repeated runs.\n\n## Related\n- semantic dedup\n- admission gate"
                    }],
                    "patterns": [],
                    "pitfalls": []
                })
                .to_string(),
            }),
            djinn_provider::provider::StreamEvent::Done,
        ],
        vec![
            djinn_provider::provider::StreamEvent::Delta(ContentBlock::Text {
                text: format!(
                    r#"{{"decision":"already_known","existing_note_id":"{}"}}"#,
                    existing.id
                ),
            }),
            djinn_provider::provider::StreamEvent::Done,
        ],
    ]));
    let _ = SEMANTIC_DUPLICATE_CANDIDATE_ID.set(existing.id.clone());
    run_llm_extraction_with_provider_and_candidate_lookup(
        fixture.session_id.clone(),
        taxonomy,
        ctx,
        provider,
        semantic_duplicate_candidate_lookup,
    )
    .await;
    let notes = note_repo
        .list(&fixture.project.id, None)
        .await
        .expect("list notes");
    let dedup_notes: Vec<_> = notes.iter().filter(|n| n.note_type == "case").collect();
    assert_eq!(dedup_notes.len(), 1);
    let updated_existing = note_repo
        .get(&existing.id)
        .await
        .expect("get existing after run")
        .expect("existing note after run");
    assert!(updated_existing.confidence > starting_confidence);
    let stored_json =
        SessionRepository::new(fixture.db.clone(), djinn_core::events::EventBus::noop())
            .get_event_taxonomy_json(&fixture.session_id)
            .await
            .expect("query session event_taxonomy after merge outcome");
    let stored_taxonomy: SessionTaxonomy = serde_json::from_str(stored_json.as_deref().unwrap())
        .expect("deserialize stored taxonomy after merge outcome");
    assert_eq!(stored_taxonomy.extraction_quality.merged, 1);
    assert_eq!(stored_taxonomy.extraction_quality.novelty_skipped, 1);
    assert_eq!(stored_taxonomy.extraction_quality.written, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_extraction_novelty_check_failure_falls_back_to_create() {
    let fixture = make_fixture().await;
    let ctx = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());
    let taxonomy = SessionTaxonomy {
        files_changed: 1,
        errors: 0,
        tools_used: 2,
        notes_read: 0,
        notes_written: 1,
        tasks_transitioned: 1,
        ..SessionTaxonomy::default()
    };
    let provider = Arc::new(FakeProvider::script(vec![
        vec![
            djinn_provider::provider::StreamEvent::Delta(ContentBlock::Text {
                text: serde_json::json!({
                    "cases": [{
                        "title": "Fallback Novel Note",
                        "content": "## Situation\nA novelty response returned invalid JSON during extraction.\n## Constraint\nThe durable lesson still matters across future tasks even when the novelty call fails.\n## Approach taken\nContinue with the structured durable case after the novelty parser falls back to unknown.\n## Result\nExtraction still captured the note instead of losing the reusable precedent.\n## Why it worked / failed\nThe fallback preserved durable note creation when novelty infrastructure was temporarily unreliable.\n## Reusable lesson\nKeep the durable write path resilient when auxiliary novelty checks fail.\n## Related\n- novelty fallback\n- durable extraction"
                    }],
                    "patterns": [],
                    "pitfalls": []
                })
                .to_string(),
            }),
            djinn_provider::provider::StreamEvent::Done,
        ],
        vec![
            djinn_provider::provider::StreamEvent::Delta(ContentBlock::Text {
                text: "not-json".to_string(),
            }),
            djinn_provider::provider::StreamEvent::Done,
        ],
    ]));
    run_llm_extraction_with_provider_and_candidate_lookup(
        fixture.session_id.clone(),
        taxonomy,
        ctx,
        provider,
        novelty_failure_candidate_lookup,
    )
    .await;
    let note_repo = fixture.note_repo();
    let notes = note_repo
        .list(&fixture.project.id, None)
        .await
        .expect("list notes");
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].title, "Fallback Novel Note");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_extraction_admission_gate_drops_pattern_with_no_required_sections() {
    // The admission gate now runs before the working-spec fallback. A pattern
    // that is missing the required ADR-054 sections and would previously have
    // been downgraded into a per-task working spec is now dropped entirely —
    // it must not be persisted at confidence 0.5 and cleaned later.
    let fixture = make_fixture().await;
    let ctx = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());
    let taxonomy = SessionTaxonomy {
        files_changed: 1,
        errors: 0,
        tools_used: 2,
        notes_read: 0,
        notes_written: 1,
        tasks_transitioned: 1,
        ..SessionTaxonomy::default()
    };
    let provider = Arc::new(FakeProvider::text(
        r#"{"cases":[],"patterns":[{"title":"Underspecified Pattern Dropped","content":"Recommended approach for this task: keep a temporary hypothesis about the current migration and maybe investigate the next step later so the team can continue the session. Why it works: it preserves context during the current task, but it is still temporary and should not become durable memory.","scope_paths":["server/crates/djinn-agent/src/actors/slot"]}],"pitfalls":[]}"#,
    ));
    run_llm_extraction_with_provider(fixture.session_id.clone(), taxonomy, ctx, provider).await;
    let note_repo = fixture.note_repo();
    let notes = note_repo
        .list(&fixture.project.id, None)
        .await
        .expect("list notes");
    assert!(
        notes.is_empty(),
        "admission gate must drop the underspecified pattern without writing any note (no working-spec fallback for malformed ADR-054 structure)"
    );
    let stored_json =
        SessionRepository::new(fixture.db.clone(), djinn_core::events::EventBus::noop())
            .get_event_taxonomy_json(&fixture.session_id)
            .await
            .expect("query session event_taxonomy after admission drop");
    let stored_taxonomy: SessionTaxonomy = serde_json::from_str(stored_json.as_deref().unwrap())
        .expect("deserialize stored taxonomy after admission drop");
    assert_eq!(stored_taxonomy.extraction_quality.extracted, 1);
    assert_eq!(stored_taxonomy.extraction_quality.admission_dropped, 1);
    assert_eq!(stored_taxonomy.extraction_quality.downgraded, 0);
    assert_eq!(stored_taxonomy.extraction_quality.written, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_extraction_admission_gate_drops_pattern_missing_adr_054_sections() {
    // The admission gate now runs before the working-spec fallback. A pattern
    // that is missing the required ADR-054 sections and would previously have
    // been downgraded into a per-task working spec is now dropped entirely.
    let fixture = make_fixture().await;
    let ctx = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());
    let taxonomy = SessionTaxonomy {
        files_changed: 1,
        errors: 1,
        tools_used: 2,
        notes_read: 0,
        notes_written: 1,
        tasks_transitioned: 1,
        ..SessionTaxonomy::default()
    };
    let provider = Arc::new(FakeProvider::text(
        serde_json::json!({
            "cases": [],
            "patterns": [{
                "title": "Unstructured Pattern Note",
                "content": "Reusable approach: keep extraction deterministic across future tasks by isolating unstable inputs and documenting why the pattern helps."
            }],
            "pitfalls": []
        }).to_string(),
    ));
    run_llm_extraction_with_provider(fixture.session_id.clone(), taxonomy, ctx, provider).await;
    let note_repo = fixture.note_repo();
    let notes = note_repo
        .list(&fixture.project.id, None)
        .await
        .expect("list notes");
    assert!(
        notes.is_empty(),
        "admission gate must drop the pattern missing ADR-054 sections without writing a working spec"
    );
    let stored_json =
        SessionRepository::new(fixture.db.clone(), djinn_core::events::EventBus::noop())
            .get_event_taxonomy_json(&fixture.session_id)
            .await
            .expect("query session event_taxonomy after admission drop");
    let stored_taxonomy: SessionTaxonomy = serde_json::from_str(stored_json.as_deref().unwrap())
        .expect("deserialize stored taxonomy after admission drop");
    assert_eq!(stored_taxonomy.extraction_quality.extracted, 1);
    assert_eq!(stored_taxonomy.extraction_quality.admission_dropped, 1);
    assert_eq!(stored_taxonomy.extraction_quality.downgraded, 0);
    assert_eq!(stored_taxonomy.extraction_quality.written, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_reflection_pipeline_structural_then_llm_extraction() {
    let fixture = make_fixture().await;
    let ctx_structural = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());
    let ctx_llm = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());
    let messages = vec![
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "write".into(),
                input: serde_json::json!({"path": "src/feature.rs", "content": "// impl"}),
            }],
            metadata: None,
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t3".into(),
                name: "memory_write".into(),
                input: serde_json::json!({"identifier": "patterns/new-pattern", "project": fixture.project.slug()}),
            }],
            metadata: None,
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t4".into(),
                name: "task_transition".into(),
                input: serde_json::json!({"task_id": fixture.task.short_id, "action": "done"}),
            }],
            metadata: None,
        },
    ];
    let taxonomy = crate::session_extraction::run_structural_extraction(
        fixture.session_id.clone(),
        messages,
        ctx_structural,
    )
    .await;
    assert!(taxonomy.is_some());
    let taxonomy = taxonomy.expect("taxonomy present");
    assert_eq!(taxonomy.files_changed, 1);
    assert_eq!(taxonomy.notes_written, 1);
    assert_eq!(taxonomy.tasks_transitioned, 1);
    fixture
        .db
        .ensure_initialized()
        .await
        .expect("db initialized");
    let stored_json =
        SessionRepository::new(fixture.db.clone(), djinn_core::events::EventBus::noop())
            .get_event_taxonomy_json(&fixture.session_id)
            .await
            .expect("query session event_taxonomy");
    assert!(stored_json.is_some());
    let stored_taxonomy: SessionTaxonomy =
        serde_json::from_str(stored_json.as_deref().expect("stored taxonomy text"))
            .expect("deserialize stored taxonomy");
    assert_eq!(stored_taxonomy.files_changed, 1);
    let provider = fake_extraction_provider();
    run_llm_extraction_with_provider(fixture.session_id.clone(), taxonomy, ctx_llm, provider).await;
    let note_repo = fixture.note_repo();
    let all_notes = note_repo
        .list(&fixture.project.id, None)
        .await
        .expect("list notes");
    assert!(!all_notes.is_empty());
    let note_types: Vec<_> = all_notes.iter().map(|n| n.note_type.as_str()).collect();
    assert!(note_types.contains(&"case"));
    assert!(note_types.contains(&"pattern"));
    assert!(note_types.contains(&"pitfall"));
    for note in &all_notes {
        assert!((note.confidence - 0.5).abs() < 1e-9);
        assert!(note.content.contains(&fixture.session_id));
    }
}

fn anchor_extraction_provider() -> Arc<FakeProvider> {
    let json = serde_json::json!({
        "cases": [{
            "title": "Anchored Case",
            "content": "## Situation\nA case must persist with a one-sentence retrieval anchor distinct from the body.\n## Constraint\nFuture tasks need an objective hook to decide when this case is the right recall.\n## Approach taken\nAdd an applies_when field and persist it into retrieval_anchor.\n## Result\nThe durable case keeps its body and gains an anchor.\n## Why it worked / failed\nThe separate field gives retrieval a focused sentence without replacing the ADR-054 body.\n## Reusable lesson\nDurable notes should be reachable by an objective situation.\n## Related\n- retrieval anchor\n- embedding",
            "applies_when": "When you need an objective situation sentence for a case note."
        }],
        "patterns": [{
            "title": "Anchored Pattern",
            "content": "## Context\nA pattern is most useful when retrieval hooks onto a situation rather than free text.\n## Problem shape\nNoisy embeddings over full content make retrieval brittle.\n## Recommended approach\nPersist a separate applies_when sentence.\n## Why it works\nThe short anchor dominates the embedding signal.\n## Tradeoffs / limits\nThe body still carries the durable lesson.\n## When to use\nUse when durable knowledge is hard to find.\n## When not to use\nDo not use when the body already is the retrieval target.\n## Related\n- retrieval anchor",
            "applies_when": "When durable knowledge is hard to retrieve by full content."
        }],
        "pitfalls": [{
            "title": "Anchored Pitfall",
            "content": "## Trigger / smell\nA pitfall becomes unrecoverable when its trigger is buried inside a long body.\n## Failure mode\nEmbedding drift hides the trigger.\n## Observable symptoms\nThe pitfall is never recalled.\n## Prevention\nPersist a separate applies_when trigger sentence.\n## Recovery\nRe-anchor and re-embed.\n## Related\n- retrieval anchor",
            "applies_when": "When a pitfall is buried inside a long body and not recalled."
        }]
    })
    .to_string();
    Arc::new(FakeProvider::text(&json))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_extraction_persists_applies_when_as_retrieval_anchor() {
    let fixture = make_fixture().await;
    let ctx = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());
    let taxonomy = SessionTaxonomy {
        files_changed: 2,
        errors: 1,
        tools_used: 4,
        notes_read: 0,
        notes_written: 1,
        tasks_transitioned: 1,
        ..SessionTaxonomy::default()
    };
    let provider = anchor_extraction_provider();
    run_llm_extraction_with_provider(fixture.session_id.clone(), taxonomy, ctx, provider).await;
    let note_repo = fixture.note_repo();
    let all_notes = note_repo
        .list(&fixture.project.id, None)
        .await
        .expect("list notes");
    let by_type: std::collections::HashMap<_, _> = all_notes
        .iter()
        .map(|n| (n.note_type.as_str(), n))
        .collect();
    let case = by_type.get("case").expect("case note written");
    let pattern = by_type.get("pattern").expect("pattern note written");
    let pitfall = by_type.get("pitfall").expect("pitfall note written");
    assert_eq!(
        case.retrieval_anchor.as_deref(),
        Some("When you need an objective situation sentence for a case note.")
    );
    assert_eq!(
        pattern.retrieval_anchor.as_deref(),
        Some("When durable knowledge is hard to retrieve by full content.")
    );
    assert_eq!(
        pitfall.retrieval_anchor.as_deref(),
        Some("When a pitfall is buried inside a long body and not recalled.")
    );
    // The anchor is distinct from the body — the body still has all ADR-054
    // sections, and the anchor is a separate persisted field.
    for note in [case, pattern, pitfall] {
        assert!(
            !note.content.contains("When you need"),
            "anchor must not be duplicated into the body"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_extraction_persists_note_without_anchor_when_model_omits_applies_when() {
    // Backward compatibility: a model that does NOT emit applies_when still
    // produces durable notes, just with a null anchor.
    let fixture = make_fixture().await;
    let ctx = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());
    let taxonomy = SessionTaxonomy::default();
    // Use the existing fake_extraction_provider (no anchor field on any note).
    let provider = fake_extraction_provider();
    run_llm_extraction_with_provider(fixture.session_id.clone(), taxonomy, ctx, provider).await;
    let note_repo = fixture.note_repo();
    let notes = note_repo
        .list(&fixture.project.id, None)
        .await
        .expect("list notes");
    assert_eq!(notes.len(), 3);
    for note in &notes {
        assert!(
            note.retrieval_anchor.is_none(),
            "missing anchor must persist as null; got {:?} on note {}",
            note.retrieval_anchor,
            note.title
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_extraction_treats_empty_anchor_as_missing() {
    // An applies_when that is whitespace-only or empty must not break extraction
    // and must be persisted as null (treated as missing). The case body is
    // built from the full ADR-054 section set so the admission gate (a separate
    // concern) does not also drop the candidate and obscure the anchor test.
    let fixture = make_fixture().await;
    let ctx = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());
    let taxonomy = SessionTaxonomy {
        files_changed: 2,
        errors: 1,
        tools_used: 4,
        notes_read: 0,
        notes_written: 1,
        tasks_transitioned: 1,
        ..SessionTaxonomy::default()
    };
    let json = serde_json::json!({
        "cases": [{
            "title": "Whitespace Anchor Case",
            "content": "## Situation\nA case that emits a whitespace-only anchor must still be persisted under a deterministic constraint.\n\n## Constraint\nThe durable lesson must remain visible across future tasks and the anchor slot must tolerate blank values from the model.\n\n## Approach taken\nIgnore the blank anchor and treat it as missing while keeping the durable body intact for retrieval.\n\n## Result\nThe note persists with a null anchor slot and the body remains durable for future readers.\n\n## Why it worked / failed\nThe anchor slot is optional in storage, so normalizing empty values to None does not break the write path.\n\n## Reusable lesson\nEmpty retrieval hooks should not break the write path; treat them as missing and persist as null.\n\n## Related\n- anchor normalization\n- admission gate",
            "applies_when": "   \n  "
        }],
        "patterns": [],
        "pitfalls": []
    })
    .to_string();
    let provider = Arc::new(FakeProvider::text(&json));
    run_llm_extraction_with_provider(fixture.session_id.clone(), taxonomy, ctx, provider).await;
    let note_repo = fixture.note_repo();
    let notes = note_repo
        .list(&fixture.project.id, None)
        .await
        .expect("list notes");
    assert_eq!(notes.len(), 1);
    assert!(
        notes[0].retrieval_anchor.is_none(),
        "whitespace-only anchor must persist as null, not empty string"
    );
}

//
// These tests cover the deterministic behavior of the extraction admission gate
// introduced by [[u7h3]]. The gate runs in `process_extracted_note` BEFORE the
// novelty judge and BEFORE `create_db_note_with_scope_and_retrieval_anchor`; a
// candidate that the shared `assess_note_quality` classifier (mofg) reports as
// `is_underspecified` is dropped with a `tracing::warn!` and increments the
// per-run `admission_dropped` counter. Passing candidates continue through the
// novelty judge and the `DUPLICATE_CONFIDENCE_SIGNAL` dedup path unchanged.
//
// Reference: server/crates/djinn-db/src/repositories/note/note_quality.rs is
// the single source of truth for the gate's `is_underspecified` decision.

/// Build a full ADR-054 case body that passes the gate: all 7 required
/// `## {section}` headings in canonical order, > 220 chars, ≥ 3 paragraphs,
/// and no underspecified markers.
fn complete_case_body() -> String {
    [
        "## Situation",
        "A deterministic extraction path needs a stable case body to reach the durable note slot.",
        "",
        "## Constraint",
        "Future tasks need a case that satisfies the gate and reaches retrieval with full provenance.",
        "",
        "## Approach taken",
        "Build the case from the canonical ADR-054 section set and pass it through the existing durable write path.",
        "",
        "## Result",
        "The case is persisted at confidence 0.5 with the session provenance footer appended to the body.",
        "",
        "## Why it worked / failed",
        "Stable body construction lets the gate focus on durability signals rather than noisy variations in tone.",
        "",
        "## Reusable lesson",
        "Stable ADR-054 case bodies are required for the durable case write path to accept the candidate and route retrieval correctly.",
        "",
        "## Related",
        "- admission gate",
        "- durable extraction",
    ]
    .join("\n")
}

/// Build a case body that has all the same shape as [`complete_case_body`]
/// but is missing the `## Reusable lesson` heading and its body line. Used
/// to isolate the missing-sections signal from the too-short-body and
/// low-paragraph signals.
fn case_body_missing_reusable_lesson() -> String {
    let full_body = complete_case_body();
    let lines: Vec<&str> = full_body.lines().collect();
    let mut pruned_lines: Vec<&str> = Vec::new();
    let mut skip_next = false;
    for line in lines {
        if skip_next {
            skip_next = false;
            continue;
        }
        if line == "## Reusable lesson" {
            // Skip the heading and the body line that immediately follows it.
            skip_next = true;
            continue;
        }
        pruned_lines.push(line);
    }
    pruned_lines.join("\n")
}

/// Build a pitfall with all required sections in order but only two `\n\n`
/// paragraphs (i.e. one continuous block between sections). Padding past the
/// 220-character floor isolates the paragraph-density signal.
fn low_paragraph_pitfall_body() -> String {
    [
        "## Trigger / smell\n## Failure mode\n## Observable symptoms\n## Prevention\n## Recovery\n## Related",
        "Combined body that is long enough to clear the 220-character floor for durable memory so only the paragraph-density signal fires here. This pitfall note still only has two paragraphs total which is below the three-paragraph minimum required for a durable note body.",
    ]
    .join("\n\n")
}

/// Resolve the `ExtractionQuality` counters persisted on the session's
/// `event_taxonomy` row after a `run_llm_extraction_*` call. The session's
/// `event_taxonomy` is the per-run metric row for the extraction path; the
/// `admission_dropped` field on it is the per-run admission-dropped counter
/// that the gate task produces and the metric task persists.
async fn extraction_quality_for(
    db: &djinn_db::Database,
    session_id: &str,
) -> super::session_extraction::ExtractionQuality {
    let stored_json = SessionRepository::new(db.clone(), djinn_core::events::EventBus::noop())
        .get_event_taxonomy_json(session_id)
        .await
        .expect("query session event_taxonomy");
    let taxonomy: SessionTaxonomy = serde_json::from_str(
        stored_json
            .as_deref()
            .expect("taxonomy persisted after gate test"),
    )
    .expect("deserialize stored taxonomy after gate test");
    taxonomy.extraction_quality
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admission_gate_passes_complete_case_note() {
    // A complete ADR-054 candidate (all 7 headings in order, > 220 chars,
    // ≥ 3 paragraphs, applies_when set, scope_paths set) must pass the gate
    // and produce exactly one durable case note with anchor and scope_paths
    // preserved.
    let fixture = make_fixture().await;
    let ctx = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());
    let taxonomy = SessionTaxonomy {
        files_changed: 2,
        errors: 1,
        tools_used: 4,
        notes_read: 0,
        notes_written: 1,
        tasks_transitioned: 1,
        ..SessionTaxonomy::default()
    };
    let case_content = complete_case_body();
    let json = serde_json::json!({
        "cases": [{
            "title": "Complete Admitted Case",
            "content": case_content,
            "applies_when": "When refactoring the call-site under latency pressure.",
            "scope_paths": ["src/db/"]
        }],
        "patterns": [],
        "pitfalls": []
    })
    .to_string();
    let provider = Arc::new(FakeProvider::text(&json));
    run_llm_extraction_with_provider(fixture.session_id.clone(), taxonomy, ctx, provider).await;
    let note_repo = fixture.note_repo();
    let notes = note_repo
        .list(&fixture.project.id, None)
        .await
        .expect("list notes");
    let cases: Vec<_> = notes.iter().filter(|n| n.note_type == "case").collect();
    assert_eq!(
        cases.len(),
        1,
        "complete ADR-054 case must produce one durable note (one create call observed)"
    );
    let created = cases[0];
    assert_eq!(created.title, "Complete Admitted Case");
    assert_eq!(
        created.retrieval_anchor.as_deref(),
        Some("When refactoring the call-site under latency pressure."),
        "applies_when must reach retrieval_anchor unchanged through the create call"
    );
    assert_eq!(
        created.parsed_scope_paths(),
        vec!["src/db/".to_string()],
        "scope_paths must reach the persisted note unchanged"
    );
    assert!(
        created.content.contains("## Reusable lesson"),
        "durable body must contain the full ADR-054 section set"
    );
    assert!(created.content.contains("## Situation"));
    let stored = extraction_quality_for(&fixture.db, &fixture.session_id).await;
    assert_eq!(stored.extracted, 1, "candidate counted as extracted");
    assert_eq!(
        stored.admission_dropped, 0,
        "complete candidate must not be dropped at the gate"
    );
    assert_eq!(stored.written, 1, "complete candidate must be written");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admission_gate_drops_case_missing_required_section() {
    // A case that is missing the `## Reusable lesson` heading must be dropped
    // at the gate: zero durable case notes, zero working-spec notes, and
    // admission_dropped == 1 on the run-metric row.
    let fixture = make_fixture().await;
    let ctx = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());
    let taxonomy = SessionTaxonomy {
        files_changed: 2,
        errors: 1,
        tools_used: 4,
        notes_read: 0,
        notes_written: 1,
        tasks_transitioned: 1,
        ..SessionTaxonomy::default()
    };
    // Build a case that is missing the `## Reusable lesson` section. We
    // remove the `## Reusable lesson` heading and its body line directly so
    // the test fixture is unambiguous about which section is missing.
    let full_body = complete_case_body();
    let lines: Vec<&str> = full_body.lines().collect();
    let mut pruned_lines: Vec<&str> = Vec::new();
    let mut skip_next = false;
    for line in lines {
        if skip_next {
            skip_next = false;
            continue;
        }
        if line == "## Reusable lesson" {
            // Skip the heading and the body line that immediately follows it.
            skip_next = true;
            continue;
        }
        pruned_lines.push(line);
    }
    let body = pruned_lines.join("\n");
    assert!(
        !body.contains("## Reusable lesson"),
        "test fixture must omit the Reusable lesson section"
    );
    assert!(
        body.contains("## Related"),
        "test fixture must keep the trailing Related section"
    );
    let json = serde_json::json!({
        "cases": [{
            "title": "Case Missing Reusable Lesson",
            "content": body,
            "applies_when": "When the case is missing the reusable lesson section."
        }],
        "patterns": [],
        "pitfalls": []
    })
    .to_string();
    let provider = Arc::new(FakeProvider::text(&json));
    let logs = CapturedLogs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(logs.clone())
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NONE)
        .with_target(false)
        .with_ansi(false)
        .with_level(true)
        .finish();
    let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
    let guard = tracing::dispatcher::set_default(&dispatch);
    run_llm_extraction_with_provider(fixture.session_id.clone(), taxonomy, ctx, provider).await;
    drop(guard);
    let captured = logs.take();
    assert!(
        captured.contains("llm_extraction: dropping underspecified note at admission gate"),
        "admission-gate rejection should emit the quality/admission diagnostic; captured: {captured}"
    );
    assert!(
        !captured.contains("llm_extraction: no LLM provider available; skipping extraction"),
        "provider-backed admission-gate rejection must not be misclassified as missing provider; captured: {captured}"
    );
    let note_repo = fixture.note_repo();
    let notes = note_repo
        .list(&fixture.project.id, None)
        .await
        .expect("list notes");
    assert!(
        notes.is_empty(),
        "case missing ## Reusable lesson must be dropped at the gate (no create call)"
    );
    let stored = extraction_quality_for(&fixture.db, &fixture.session_id).await;
    assert_eq!(stored.extracted, 1);
    assert_eq!(
        stored.admission_dropped, 1,
        "run-metric row must record the drop"
    );
    assert_eq!(stored.written, 0);
    assert_eq!(
        stored.downgraded, 0,
        "dropped candidates must not flow into the working-spec fallback"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admission_gate_drops_short_body_note() {
    // A case with all required headings but a body shorter than 220 chars
    // must be dropped at the gate. The fixture keeps every `## {section}`
    // heading (in order) so the missing-sections signal does NOT fire — only
    // the too-short-body signal does.
    let fixture = make_fixture().await;
    let ctx = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());
    let taxonomy = SessionTaxonomy {
        files_changed: 2,
        errors: 1,
        tools_used: 4,
        notes_read: 0,
        notes_written: 1,
        tasks_transitioned: 1,
        ..SessionTaxonomy::default()
    };
    // All 7 case headings, with a body that is well under 220 chars.
    let short_body = "## Situation\nshort.\n## Constraint\nshort.\n## Approach taken\nshort.\n## Result\nshort.\n## Why it worked / failed\nshort.\n## Reusable lesson\nshort.\n## Related\n- short.";
    assert!(
        short_body.chars().count() < 220,
        "fixture must be short enough to trip the too-short-body signal"
    );
    let json = serde_json::json!({
        "cases": [{
            "title": "Short Body Case",
            "content": short_body
        }],
        "patterns": [],
        "pitfalls": []
    })
    .to_string();
    let provider = Arc::new(FakeProvider::text(&json));
    run_llm_extraction_with_provider(fixture.session_id.clone(), taxonomy, ctx, provider).await;
    let note_repo = fixture.note_repo();
    let notes = note_repo
        .list(&fixture.project.id, None)
        .await
        .expect("list notes");
    assert!(
        notes.is_empty(),
        "short-body case must be dropped at the gate (no create call)"
    );
    let stored = extraction_quality_for(&fixture.db, &fixture.session_id).await;
    assert_eq!(stored.extracted, 1);
    assert_eq!(
        stored.admission_dropped, 1,
        "run-metric must record the drop"
    );
    assert_eq!(stored.written, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admission_gate_drops_low_paragraph_note() {
    // A pitfall with all required headings in order and a body long enough to
    // clear the 220-char floor but only two `\n\n`-delimited paragraphs must
    // be dropped at the gate. The fixture isolates the paragraph-density
    // signal from the too-short-body signal.
    let fixture = make_fixture().await;
    let ctx = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());
    let taxonomy = SessionTaxonomy {
        files_changed: 2,
        errors: 1,
        tools_used: 4,
        notes_read: 0,
        notes_written: 1,
        tasks_transitioned: 1,
        ..SessionTaxonomy::default()
    };
    let body = low_paragraph_pitfall_body();
    assert!(
        body.chars().count() >= 220,
        "fixture must clear the 220-char floor so only the paragraph signal fires"
    );
    let json = serde_json::json!({
        "cases": [],
        "patterns": [],
        "pitfalls": [{
            "title": "Low Paragraph Pitfall",
            "content": body
        }]
    })
    .to_string();
    let provider = Arc::new(FakeProvider::text(&json));
    run_llm_extraction_with_provider(fixture.session_id.clone(), taxonomy, ctx, provider).await;
    let note_repo = fixture.note_repo();
    let notes = note_repo
        .list(&fixture.project.id, None)
        .await
        .expect("list notes");
    assert!(
        notes.is_empty(),
        "low-paragraph pitfall must be dropped at the gate (no create call)"
    );
    let stored = extraction_quality_for(&fixture.db, &fixture.session_id).await;
    assert_eq!(stored.extracted, 1);
    assert_eq!(
        stored.admission_dropped, 1,
        "run-metric must record the drop for the low-paragraph pitfall"
    );
    assert_eq!(stored.written, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admission_gate_preserves_applies_when_and_scope_paths() {
    // A passing candidate with an applies_when and scope_paths must reach
    // `create_db_note_with_scope_and_retrieval_anchor` unchanged. The gate
    // runs first and only inspects the body — it must not perturb the
    // retrieval_anchor or scope_paths the model returned.
    let fixture = make_fixture().await;
    let ctx = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());
    let taxonomy = SessionTaxonomy {
        files_changed: 2,
        errors: 1,
        tools_used: 4,
        notes_read: 0,
        notes_written: 1,
        tasks_transitioned: 1,
        ..SessionTaxonomy::default()
    };
    let json = serde_json::json!({
        "cases": [{
            "title": "Anchored Passing Case",
            "content": complete_case_body(),
            "applies_when": "When refactoring the call-site under latency pressure.",
            "scope_paths": ["src/db/"]
        }],
        "patterns": [],
        "pitfalls": []
    })
    .to_string();
    let provider = Arc::new(FakeProvider::text(&json));
    run_llm_extraction_with_provider(fixture.session_id.clone(), taxonomy, ctx, provider).await;
    let note_repo = fixture.note_repo();
    let notes = note_repo
        .list(&fixture.project.id, None)
        .await
        .expect("list notes");
    let cases: Vec<_> = notes.iter().filter(|n| n.note_type == "case").collect();
    assert_eq!(cases.len(), 1, "one durable case must be created");
    let created = cases[0];
    assert_eq!(
        created.retrieval_anchor.as_deref(),
        Some("When refactoring the call-site under latency pressure."),
        "applies_when must reach retrieval_anchor unchanged through the create call"
    );
    assert_eq!(
        created.parsed_scope_paths(),
        vec!["src/db/".to_string()],
        "scope_paths must reach the persisted note unchanged"
    );
    let stored = extraction_quality_for(&fixture.db, &fixture.session_id).await;
    assert_eq!(
        stored.admission_dropped, 0,
        "passing candidate must not be dropped"
    );
    assert_eq!(stored.written, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admission_gate_preserves_novelty_dedup() {
    // A passing candidate that the novelty judge reports as `AlreadyKnown`
    // must NOT create a new note; the existing note's confidence is updated
    // via the existing `DUPLICATE_CONFIDENCE_SIGNAL` path. The gate runs
    // first and only inspects the body — it must not disturb the dedup
    // signal. Drop counter for this candidate must remain 0.
    let fixture = make_fixture().await;
    let ctx = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());
    let note_repo = fixture.note_repo();
    let existing = note_repo
        .create_db_note(
            &fixture.project.id,
            "Existing Anchor Target",
            "Existing content",
            "case",
            "[]",
        )
        .await
        .expect("create existing note");
    note_repo
        .set_confidence(&existing.id, 0.5)
        .await
        .expect("set starting confidence");
    let starting_confidence = note_repo
        .get(&existing.id)
        .await
        .expect("get existing before run")
        .expect("existing note before run")
        .confidence;
    let taxonomy = SessionTaxonomy {
        files_changed: 2,
        errors: 1,
        tools_used: 4,
        notes_read: 0,
        notes_written: 1,
        tasks_transitioned: 1,
        ..SessionTaxonomy::default()
    };
    let provider = Arc::new(FakeProvider::script(vec![
        vec![
            djinn_provider::provider::StreamEvent::Delta(ContentBlock::Text {
                text: serde_json::json!({
                    "cases": [{
                        "title": "Passing Duplicate Case",
                        "content": complete_case_body(),
                        "applies_when": "When the dedup path is reached for a complete case.",
                        "scope_paths": ["src/db/"]
                    }],
                    "patterns": [],
                    "pitfalls": []
                })
                .to_string(),
            }),
            djinn_provider::provider::StreamEvent::Done,
        ],
        vec![
            djinn_provider::provider::StreamEvent::Delta(ContentBlock::Text {
                text: format!(
                    r#"{{"decision":"already_known","existing_note_id":"{}"}}"#,
                    existing.id
                ),
            }),
            djinn_provider::provider::StreamEvent::Done,
        ],
    ]));
    // Override the candidate lookup to surface the existing note as the only
    // semantic-duplicate candidate, so the novelty judge picks it.
    let _ = SEMANTIC_DUPLICATE_CANDIDATE_ID.set(existing.id.clone());
    let lookup: fn(&str, &str, &str, &str) -> Vec<djinn_db::NoteDedupCandidate> =
        |_project_id, _folder, _note_type, _candidate_abstract| {
            let id = SEMANTIC_DUPLICATE_CANDIDATE_ID
                .get()
                .expect("semantic duplicate candidate id configured")
                .clone();
            vec![NoteDedupCandidate {
                id,
                permalink: "cases/existing-anchor-target".to_string(),
                title: "Existing Anchor Target".to_string(),
                folder: "cases".to_string(),
                note_type: "case".to_string(),
                content: "Full existing anchor target body.".to_string(),
                abstract_: Some("Existing case for the gate-passing dedup test.".to_string()),
                overview: Some(
                    "An existing case is the dedup target for a passing candidate.".to_string(),
                ),
                score: 1.0,
            }]
        };
    run_llm_extraction_with_provider_and_candidate_lookup(
        fixture.session_id.clone(),
        taxonomy,
        ctx,
        provider,
        lookup,
    )
    .await;
    let notes = note_repo
        .list(&fixture.project.id, None)
        .await
        .expect("list notes");
    let case_notes: Vec<_> = notes.iter().filter(|n| n.note_type == "case").collect();
    assert_eq!(
        case_notes.len(),
        1,
        "dedup must NOT create a new case; only the existing note remains"
    );
    assert_eq!(case_notes[0].id, existing.id);
    let updated_existing = note_repo
        .get(&existing.id)
        .await
        .expect("get existing after run")
        .expect("existing note after run");
    assert!(
        updated_existing.confidence > starting_confidence,
        "DUPLICATE_CONFIDENCE_SIGNAL must boost the existing note's confidence"
    );
    let stored = extraction_quality_for(&fixture.db, &fixture.session_id).await;
    assert_eq!(
        stored.admission_dropped, 0,
        "passing candidate must not be dropped at the gate"
    );
    assert_eq!(stored.merged, 1, "dedup must increment the merged counter");
    assert_eq!(stored.novelty_skipped, 1);
    assert_eq!(stored.written, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admission_gate_increments_metric_for_each_drop() {
    // A mixed batch of 5 candidates (3 complete, 1 missing a section, 1
    // short-body) must produce 3 durable notes and an `admission_dropped`
    // counter of 2.
    let fixture = make_fixture().await;
    let ctx = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());
    let taxonomy = SessionTaxonomy {
        files_changed: 4,
        errors: 2,
        tools_used: 8,
        notes_read: 0,
        notes_written: 3,
        tasks_transitioned: 1,
        ..SessionTaxonomy::default()
    };
    // Two of the three "complete" cases are intentionally not in this list —
    // the third is the short-body case whose `## Situation` body alone clears
    // the 220-char floor is too tricky to construct without also tripping the
    // missing-sections signal, so we cover the 3-complete + 1-missing +
    // 1-short mix with one section-missing and one short-body drop.
    let complete_pattern_body = [
        "## Context",
        "A pattern that satisfies the gate and reaches durable storage.",
        "",
        "## Problem shape",
        "Unstable inputs make the durable write path flaky across repeated runs.",
        "",
        "## Recommended approach",
        "Inject a stable candidate seam and keep the comparison summary explicit.",
        "",
        "## Why it works",
        "The seam isolates the durable body from noisy inputs and preserves reuse across future tasks.",
        "",
        "## Tradeoffs / limits",
        "Adds test scaffolding; only helps when the comparison boundary is well understood.",
        "",
        "## When to use",
        "When the durable pattern must reach retrieval deterministically across repeated runs.",
        "",
        "## When not to use",
        "Do not use when the comparison boundary is still changing or the workflow is exploratory.",
        "",
        "## Related",
        "- durable extraction",
    ]
    .join("\n");
    let json = serde_json::json!({
        "cases": [
            {
                "title": "Complete Case A",
                "content": complete_case_body(),
                "applies_when": "When case A is the durable precedent.",
                "scope_paths": ["src/a/"]
            },
            {
                "title": "Complete Case B",
                "content": complete_case_body(),
                "applies_when": "When case B is the durable precedent.",
                "scope_paths": ["src/b/"]
            },
            {
                "title": "Case Missing Section",
                "content": case_body_missing_reusable_lesson(),
                "applies_when": "When a case omits the reusable lesson."
            }
        ],
        "patterns": [
            {
                "title": "Complete Pattern A",
                "content": complete_pattern_body,
                "applies_when": "When durable pattern retrieval must be deterministic.",
                "scope_paths": ["src/pattern/"]
            }
        ],
        "pitfalls": [
            {
                "title": "Short Pitfall",
                "content": "## Trigger / smell\nx\n## Failure mode\nx\n## Observable symptoms\nx\n## Prevention\nx\n## Recovery\nx\n## Related\nx"
            }
        ]
    })
    .to_string();
    let provider = Arc::new(FakeProvider::text(&json));
    run_llm_extraction_with_provider(fixture.session_id.clone(), taxonomy, ctx, provider).await;
    let note_repo = fixture.note_repo();
    let notes = note_repo
        .list(&fixture.project.id, None)
        .await
        .expect("list notes");
    // 3 complete candidates (case A, case B, pattern A) must reach durable
    // storage; the missing-section case and the short pitfall must be dropped.
    let durable_count = notes
        .iter()
        .filter(|n| matches!(n.note_type.as_str(), "case" | "pattern" | "pitfall"))
        .count();
    assert_eq!(
        durable_count, 3,
        "exactly 3 candidates (case A, case B, pattern A) must be written; 2 are dropped"
    );
    let stored = extraction_quality_for(&fixture.db, &fixture.session_id).await;
    assert_eq!(
        stored.extracted, 5,
        "all 5 unique candidates counted as extracted"
    );
    assert_eq!(
        stored.admission_dropped, 2,
        "run-metric admission_dropped must equal 2 (missing section + short body)"
    );
    assert_eq!(
        stored.written, 3,
        "exactly 3 candidates reach the durable write path"
    );
    assert_eq!(
        stored.downgraded, 0,
        "no working-spec fallback for dropped candidates"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admission_gate_does_not_affect_human_writes() {
    // The gate lives inside `run_llm_extraction_inner`. A non-extraction call
    // to `note_repo.create_db_note_with_scope_and_retrieval_anchor` is
    // outside the gate's scope and must be unaffected: it writes the note
    // directly with the supplied anchor and scope_paths and the gate's
    // `admission_dropped` counter remains at zero because no extraction run
    // happens in this test.
    let fixture = make_fixture().await;
    let note_repo = fixture.note_repo();
    let created = note_repo
        .create_db_note_with_scope_and_retrieval_anchor(
            &fixture.project.id,
            "Human Authored Note",
            "## Some\nHuman-written body that does NOT satisfy the ADR-054 gate; human writes are not gated.",
            "reference",
            "[]",
            "[\"src/manual/\"]",
            Some("Human-authored retrieval anchor."),
        )
        .await
        .expect("human-authored note creates regardless of the gate");
    assert_eq!(created.title, "Human Authored Note");
    assert_eq!(
        created.retrieval_anchor.as_deref(),
        Some("Human-authored retrieval anchor.")
    );
    assert_eq!(
        created.parsed_scope_paths(),
        vec!["src/manual/".to_string()]
    );
    // No `run_llm_extraction_*` call happens in this test, so the
    // `event_taxonomy` for this session does not exist and there is no
    // `admission_dropped` counter to consult. The above note creation IS
    // the assertion: the gate's logic is not applied to this path.
    let session_repo =
        SessionRepository::new(fixture.db.clone(), djinn_core::events::EventBus::noop());
    let stored_json = session_repo
        .get_event_taxonomy_json(&fixture.session_id)
        .await
        .expect("query session event_taxonomy");
    assert!(
        stored_json.is_none(),
        "no extraction run means no admission_dropped counter is written"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_with_gate_drops_surfaces_nonzero_admission_dropped_in_health() {
    // Run extraction with a case missing `## Reusable lesson` (same fixture
    // shape as `admission_gate_drops_case_missing_required_section`). After
    // extraction, call `NoteRepository::health()` and assert that the
    // `admission_dropped_note_count` is >= 1 — proving the metric row was
    // written from the extraction path and is surfaced via health.
    let fixture = make_fixture().await;
    let ctx = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());
    let taxonomy = SessionTaxonomy {
        files_changed: 2,
        errors: 1,
        tools_used: 4,
        notes_read: 0,
        notes_written: 1,
        tasks_transitioned: 1,
        ..SessionTaxonomy::default()
    };
    let body = case_body_missing_reusable_lesson();
    let json = serde_json::json!({
        "cases": [{
            "title": "Dropped In Health Case",
            "content": body
        }],
        "patterns": [],
        "pitfalls": []
    })
    .to_string();
    let provider = Arc::new(FakeProvider::text(&json));
    run_llm_extraction_with_provider(fixture.session_id.clone(), taxonomy, ctx, provider).await;
    let note_repo = fixture.note_repo();
    let health = note_repo
        .health(&fixture.project.id)
        .await
        .expect("health report");
    assert!(
        health.admission_dropped_note_count >= 1,
        "health() must surface a non-zero admission_dropped_note_count when the gate drops a candidate, got {}",
        health.admission_dropped_note_count
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_with_zero_gate_drops_writes_zero_admission_dropped_metric() {
    // Run extraction with one complete case that passes the gate. Read the
    // emitted consolidation_run_metrics row and assert admission_dropped is 0.
    // Also assert health() reports 0.
    let fixture = make_fixture().await;
    let ctx = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());
    let taxonomy = SessionTaxonomy {
        files_changed: 2,
        errors: 1,
        tools_used: 4,
        notes_read: 0,
        notes_written: 1,
        tasks_transitioned: 1,
        ..SessionTaxonomy::default()
    };
    let json = serde_json::json!({
        "cases": [{
            "title": "Complete No-Drop Case",
            "content": complete_case_body(),
            "applies_when": "When a complete case passes the gate cleanly."
        }],
        "patterns": [],
        "pitfalls": []
    })
    .to_string();
    let provider = Arc::new(FakeProvider::text(&json));
    run_llm_extraction_with_provider(fixture.session_id.clone(), taxonomy, ctx, provider).await;
    let consolidation_repo = NoteConsolidationRepository::new(fixture.db.clone());
    let metrics = consolidation_repo
        .list_run_metrics(&fixture.project.id, Some("extraction"), 10)
        .await
        .expect("list run metrics");
    let extraction_metric = metrics
        .iter()
        .find(|m| m.note_type == "extraction")
        .expect("at least one extraction metric row must be written");
    assert_eq!(
        extraction_metric.admission_dropped_note_count, 0,
        "admission_dropped_note_count must be 0 when no candidates are dropped"
    );
    let note_repo = fixture.note_repo();
    let health = note_repo
        .health(&fixture.project.id)
        .await
        .expect("health report");
    assert_eq!(
        health.admission_dropped_note_count, 0,
        "health() must report 0 admission drops when no candidates were dropped"
    );
}
