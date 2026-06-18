#![allow(clippy::too_many_lines)]
// Tests for the full reflection pipeline and extracted-note persistence.
//!
//! Covers:
//! AC1 - Session completion triggers full reflection pipeline
//! AC2 - Structural extraction produces co-access pairs and event taxonomy
//! AC3 - LLM extraction with FakeProvider produces case/pattern/pitfall notes
//! AC4 - Extracted notes have confidence 0.5 and session provenance in content
//! AC5 - Graceful degradation: LLM unavailable → no notes written, no errors
//! AC6 - Dedup pipeline: repeated sessions do not create duplicate notes

use std::sync::Arc;

use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use djinn_core::message::{ContentBlock, Message, Role};
use djinn_db::{
    CreateSessionParams, EpicCreateInput, EpicRepository, NoteDedupCandidate, NoteRepository,
    ProjectRepository, SessionRepository, TaskRepository,
};

use crate::actors::slot::llm_extraction::{
    run_llm_extraction, run_llm_extraction_with_provider,
    run_llm_extraction_with_provider_and_candidate_lookup,
};
use crate::actors::slot::session_extraction::{SessionTaxonomy, extract_session_signals};
use crate::test_helpers::{FailingProvider, FakeProvider, agent_context_from_db, create_test_db};

// ─── Test helpers ─────────────────────────────────────────────────────────────

/// Creates a temp directory (notes will be written there).
fn make_tmpdir() -> TempDir {
    crate::test_helpers::test_tempdir("djinn-llm-extraction-")
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
        abstract_: Some("Existing candidate summary".to_string()),
        overview: Some("Existing candidate overview".to_string()),
        score: 1.0,
    }]
}

// ─── AC2: Structural extraction ────────────────────────────────────────────────

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

    let events = djinn_core::events::EventBus::noop();
    let note_repo = NoteRepository::new(fixture.db.clone(), events.clone());

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

    let taxonomy = crate::actors::slot::session_extraction::run_structural_extraction(
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

    let note_repo = NoteRepository::new(fixture.db.clone(), djinn_core::events::EventBus::noop());
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

    let note_repo = NoteRepository::new(fixture.db.clone(), djinn_core::events::EventBus::noop());
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

    let note_repo = NoteRepository::new(fixture.db.clone(), djinn_core::events::EventBus::noop());
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

    let note_repo = NoteRepository::new(fixture.db.clone(), djinn_core::events::EventBus::noop());
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

    let empty_note_repo = NoteRepository::new(
        empty_fixture.db.clone(),
        djinn_core::events::EventBus::noop(),
    );
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

    let failed_note_repo = NoteRepository::new(
        failed_fixture.db.clone(),
        djinn_core::events::EventBus::noop(),
    );
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

    let note_repo = NoteRepository::new(fixture.db.clone(), djinn_core::events::EventBus::noop());
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
    run_llm_extraction(fixture.session_id.clone(), taxonomy, ctx).await;

    let note_repo = NoteRepository::new(fixture.db.clone(), djinn_core::events::EventBus::noop());
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
    let note_repo = NoteRepository::new(fixture.db.clone(), djinn_core::events::EventBus::noop());

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
                        "content": "## Situation
Flaky semantic duplicate tests made extraction unreliable across repeated runs.
## Constraint
Novelty checks needed stable inputs and deterministic comparison summaries.
## Approach taken
Inject dedup candidates with stable summaries and compare them deterministically.
## Result
The extraction pipeline stopped creating noisy duplicate notes.
## Why it worked / failed
Stable comparison inputs removed the non-determinism that caused flaky merges.
## Reusable lesson
Use injected stable candidates and deterministic summaries when testing novelty detection.
## Related
- semantic dedup
- extraction quality"
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

    let note_repo = NoteRepository::new(fixture.db.clone(), djinn_core::events::EventBus::noop());
    let notes = note_repo
        .list(&fixture.project.id, None)
        .await
        .expect("list notes");

    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].title, "Fallback Novel Note");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_extraction_downgrades_non_durable_note_to_working_spec_path() {
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
        &serde_json::json!({
            "cases": [],
            "patterns": [{
                "title": "Temporary Working Spec Note",
                "content": "## Context
A working hypothesis about the current migration that needs investigation before the next step. The approach is temporary and might change.
## Problem shape
The current task requires a temporary solution that could change soon.
## Recommended approach
Investigate the next step and maybe adjust the approach. This is for now.
## Why it works
The hypothesis preserves context during the current task and temporary work.
## Tradeoffs / limits
This is a temporary approach that might not work long-term. The current task focus limits reuse.
## When to use
Use when the current task needs a working hypothesis and temporary investigation.
## When not to use
Do not use when a durable lesson is available or the approach is stable.
## Related
- temporary approaches
- working hypotheses",
                "scope_paths": ["server/crates/djinn-agent/src/actors/slot"]
            }],
            "pitfalls": []
        }).to_string(),
    ));

    run_llm_extraction_with_provider(fixture.session_id.clone(), taxonomy, ctx, provider).await;

    let note_repo = NoteRepository::new(fixture.db.clone(), djinn_core::events::EventBus::noop());
    let notes = note_repo
        .list(&fixture.project.id, None)
        .await
        .expect("list notes");
    assert_eq!(
        notes.len(),
        1,
        "downgraded note should be retained as a working spec"
    );
    let working_spec = &notes[0];
    assert_eq!(working_spec.note_type, "design");
    assert_eq!(
        working_spec.title,
        format!("Working Spec {}", fixture.task.short_id)
    );
    assert!(working_spec.content.contains("## Active objective"));
    assert!(working_spec.content.contains("## Relevant scope"));
    assert!(working_spec.content.contains("## Constraints"));
    assert!(working_spec.content.contains("## Current hypotheses"));
    assert!(working_spec.content.contains("## Open questions"));
    assert!(working_spec.content.contains("Temporary Working Spec Note"));
    assert!(working_spec.content.contains("task-scoped working context"));
    assert!(working_spec.content.contains(&fixture.session_id));
    assert!(working_spec.folder.starts_with("design"));

    let durable_notes: Vec<_> = notes
        .iter()
        .filter(|note| matches!(note.note_type.as_str(), "case" | "pattern" | "pitfall"))
        .collect();
    assert!(
        durable_notes.is_empty(),
        "downgraded notes should not become durable extracted notes"
    );

    let stored_json =
        SessionRepository::new(fixture.db.clone(), djinn_core::events::EventBus::noop())
            .get_event_taxonomy_json(&fixture.session_id)
            .await
            .expect("query session event_taxonomy after downgrade");
    let stored_taxonomy: SessionTaxonomy = serde_json::from_str(stored_json.as_deref().unwrap())
        .expect("deserialize stored taxonomy after downgrade");
    assert_eq!(stored_taxonomy.extraction_quality.extracted, 1);
    assert_eq!(stored_taxonomy.extraction_quality.downgraded, 1);
    assert_eq!(stored_taxonomy.extraction_quality.written, 0);
    assert_eq!(stored_taxonomy.extraction_quality.admission_dropped, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_extraction_downgrades_note_missing_required_adr_054_sections() {
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
        &serde_json::json!({
            "cases": [],
            "patterns": [{
                "title": "Unstructured Pattern Note",
                "content": "Reusable approach: keep extraction deterministic across future tasks by isolating unstable inputs and documenting why the pattern helps."
            }],
            "pitfalls": []
        }).to_string(),
    ));

    run_llm_extraction_with_provider(fixture.session_id.clone(), taxonomy, ctx, provider).await;

    let note_repo = NoteRepository::new(fixture.db.clone(), djinn_core::events::EventBus::noop());
    let notes = note_repo
        .list(&fixture.project.id, None)
        .await
        .expect("list notes");
    // The ADR-054 admission gate drops underspecified notes before novelty
    // judging and before any create_* call — no working-spec fallback, no
    // durable write. The note is simply skipped.
    assert!(
        notes.is_empty(),
        "notes missing all ADR-054 sections should be dropped at admission gate, not persisted"
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

    let taxonomy = crate::actors::slot::session_extraction::run_structural_extraction(
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

    let note_repo = NoteRepository::new(fixture.db.clone(), djinn_core::events::EventBus::noop());
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

// ─── AC7: applies_when anchor is persisted on durable extracted notes ─────────

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

    let note_repo = NoteRepository::new(fixture.db.clone(), djinn_core::events::EventBus::noop());
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

    let note_repo = NoteRepository::new(fixture.db.clone(), djinn_core::events::EventBus::noop());
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
    // and must be persisted as null (treated as missing).
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
            "content": "## Situation\nA case that emits a whitespace-only anchor must still be persisted under a deterministic constraint.\n## Constraint\nThe durable lesson must remain visible across future tasks even when the optional retrieval hook is blank.\n## Approach taken\nIgnore the blank anchor after trimming and treat it as missing while leaving the durable note body intact.\n## Result\nThe note persists successfully with a null retrieval anchor instead of an empty string.\n## Why it worked / failed\nThe anchor normalization is separate from ADR-054 body validation, so empty optional metadata does not invalidate reusable content.\n## Reusable lesson\nEmpty retrieval hooks should be normalized away without breaking durable note writes.\n## Related\n- anchor normalization",
            "applies_when": "   \n  "
        }],
        "patterns": [],
        "pitfalls": []
    })
    .to_string();
    let provider = Arc::new(FakeProvider::text(&json));
    run_llm_extraction_with_provider(fixture.session_id.clone(), taxonomy, ctx, provider).await;

    let note_repo = NoteRepository::new(fixture.db.clone(), djinn_core::events::EventBus::noop());
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
