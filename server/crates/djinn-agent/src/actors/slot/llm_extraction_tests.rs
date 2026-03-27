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
    let project = project_repo
        .create(
            &format!("test-project-{uid}"),
            tmpdir.path().to_str().expect("tmpdir path to str"),
        )
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
            worktree_path: None,
            metadata_json: None,
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
    let json = r#"{
  "cases": [{"title": "Test Case Note", "content": "A test case: problem and solution described here."}],
  "patterns": [{"title": "Test Pattern Note", "content": "A reusable pattern discovered in this session."}],
  "pitfalls": [{"title": "Test Pitfall Note", "content": "A pitfall encountered and how it was resolved."}]
}"#;
    Arc::new(FakeProvider::text(json))
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
                name: "git_commit".into(),
                input: serde_json::json!({"message": "implement feature"}),
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
                name: "git_push".into(),
                input: serde_json::json!({}),
            }],
            metadata: None,
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t5".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/main.rs", "content": "fn main() {}"}),
            }],
            metadata: None,
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t6".into(),
                name: "task_transition".into(),
                input: serde_json::json!({"task_id": "abc", "action": "done"}),
            }],
            metadata: None,
        },
    ];

    let (taxonomy, notes_read, _stale) = extract_session_signals(&messages);

    assert_eq!(taxonomy.notes_read, 2);
    assert_eq!(taxonomy.git_ops, 2);
    assert_eq!(taxonomy.errors, 1);
    assert_eq!(taxonomy.files_changed, 1);
    assert_eq!(taxonomy.tasks_transitioned, 1);
    assert_eq!(taxonomy.tools_used, 5);
    assert_eq!(notes_read.len(), 2);
    assert!(notes_read.contains(&"decisions/adr-001".to_string()));
    assert!(notes_read.contains(&"decisions/adr-002".to_string()));
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
            fixture.tmpdir.path(),
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
            fixture.tmpdir.path(),
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
                    "project": fixture.project.path
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
                    "project": fixture.project.path
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
        git_ops: 1,
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
        git_ops: 1,
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

    let stored_json: Option<String> =
        sqlx::query_scalar("SELECT event_taxonomy FROM sessions WHERE id = ?1")
            .bind(&fixture.session_id)
            .fetch_one(fixture.db.pool())
            .await
            .expect("query session event_taxonomy after llm extraction");
    let stored_taxonomy: SessionTaxonomy = serde_json::from_str(stored_json.as_deref().unwrap())
        .expect("deserialize stored taxonomy after llm extraction");
    assert_eq!(stored_taxonomy.extraction_quality.extracted, 3);
    assert_eq!(stored_taxonomy.extraction_quality.dedup_skipped, 0);
    assert_eq!(stored_taxonomy.extraction_quality.novelty_skipped, 0);
    assert_eq!(stored_taxonomy.extraction_quality.written, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_extracted_notes_contain_session_id_provenance() {
    let fixture = make_fixture().await;
    let session_id = fixture.session_id.clone();
    let ctx = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());

    let taxonomy = SessionTaxonomy::default();
    let json = r#"{"cases":[{"title":"Provenance Test","content":"This content should have provenance."}],"patterns":[],"pitfalls":[]}"#;
    let provider = Arc::new(FakeProvider::text(json));

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
        git_ops: 2,
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
async fn llm_extraction_graceful_degradation_no_provider_configured() {
    let fixture = make_fixture().await;
    let ctx = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());

    let taxonomy = SessionTaxonomy {
        files_changed: 2,
        errors: 0,
        git_ops: 1,
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
        git_ops: 1,
        tools_used: 4,
        notes_read: 0,
        notes_written: 1,
        tasks_transitioned: 1,
        ..SessionTaxonomy::default()
    };

    let provider = Arc::new(FakeProvider::script(vec![
        vec![
            djinn_provider::provider::StreamEvent::Delta(ContentBlock::Text {
                text: r#"{"cases":[{"title":"Duplicate Semantic Note","content":"Fix flaky semantic duplicate tests by injecting dedup candidates and comparing stable summaries."}],"patterns":[],"pitfalls":[]}"#.to_string(),
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_extraction_novelty_check_failure_falls_back_to_create() {
    let fixture = make_fixture().await;
    let ctx = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());

    let taxonomy = SessionTaxonomy {
        files_changed: 1,
        errors: 0,
        git_ops: 1,
        tools_used: 2,
        notes_read: 0,
        notes_written: 1,
        tasks_transitioned: 1,
        ..SessionTaxonomy::default()
    };

    let provider = Arc::new(FakeProvider::script(vec![
        vec![
            djinn_provider::provider::StreamEvent::Delta(ContentBlock::Text {
                text: r#"{"cases":[{"title":"Fallback Novel Note","content":"Should still be written when novelty check output is invalid."}],"patterns":[],"pitfalls":[]}"#.to_string(),
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
async fn full_reflection_pipeline_structural_then_llm_extraction() {
    let fixture = make_fixture().await;
    let ctx_structural = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());
    let ctx_llm = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());

    let messages = vec![
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/feature.rs", "content": "// impl"}),
            }],
            metadata: None,
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t2".into(),
                name: "git_commit".into(),
                input: serde_json::json!({"message": "feat: implement feature"}),
            }],
            metadata: None,
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t3".into(),
                name: "memory_write".into(),
                input: serde_json::json!({"identifier": "patterns/new-pattern", "project": fixture.project.path}),
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
    assert_eq!(taxonomy.git_ops, 1);
    assert_eq!(taxonomy.notes_written, 1);
    assert_eq!(taxonomy.tasks_transitioned, 1);

    fixture
        .db
        .ensure_initialized()
        .await
        .expect("db initialized");
    let stored_json: Option<String> =
        sqlx::query_scalar("SELECT event_taxonomy FROM sessions WHERE id = ?1")
            .bind(&fixture.session_id)
            .fetch_one(fixture.db.pool())
            .await
            .expect("query session event_taxonomy");

    assert!(stored_json.is_some());
    let stored_taxonomy: SessionTaxonomy =
        serde_json::from_str(stored_json.as_deref().expect("stored taxonomy text"))
            .expect("deserialize stored taxonomy");
    assert_eq!(stored_taxonomy.files_changed, 1);
    assert_eq!(stored_taxonomy.git_ops, 1);

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
