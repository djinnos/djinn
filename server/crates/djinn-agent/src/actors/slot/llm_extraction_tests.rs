//! Integration tests for the session reflection pipeline.
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
    CreateSessionParams, EpicCreateInput, EpicRepository, NoteRepository, ProjectRepository,
    SessionRepository, TaskRepository,
};

use crate::actors::slot::llm_extraction::{run_llm_extraction, run_llm_extraction_with_provider};
use crate::actors::slot::session_extraction::{SessionTaxonomy, extract_session_signals};
use crate::test_helpers::{FailingProvider, FakeProvider, agent_context_from_db, create_test_db};

// ─── Test helpers ─────────────────────────────────────────────────────────────

/// Creates a temp directory (notes will be written there).
fn make_tmpdir() -> TempDir {
    tempfile::tempdir().expect("tempdir")
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
            tmpdir.path().to_str().unwrap(),
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

// ─── AC2: Structural extraction ────────────────────────────────────────────────

/// AC2 part 1: structural extraction produces correct event taxonomy from messages.
#[test]
fn structural_extraction_produces_correct_taxonomy() {
    // Build messages: 2 memory_reads, 2 git ops, 1 error, 1 file write, 1 task_transition
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

    assert_eq!(taxonomy.notes_read, 2, "should count 2 memory_reads");
    assert_eq!(taxonomy.git_ops, 2, "should count git_commit + git_push");
    assert_eq!(taxonomy.errors, 1, "should count 1 tool error");
    assert_eq!(taxonomy.files_changed, 1, "should count 1 unique file");
    assert_eq!(
        taxonomy.tasks_transitioned, 1,
        "should count 1 task_transition"
    );
    assert_eq!(taxonomy.tools_used, 5, "should count 5 unique tool names");
    assert_eq!(notes_read.len(), 2);
    assert!(notes_read.contains(&"decisions/adr-001".to_string()));
    assert!(notes_read.contains(&"decisions/adr-002".to_string()));
}

/// AC2 part 2: structural extraction updates co-access associations in DB.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn structural_extraction_flushes_co_access_associations() {
    let fixture = make_fixture().await;
    let ctx = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());

    let events = djinn_core::events::EventBus::noop();
    let note_repo = NoteRepository::new(fixture.db.clone(), events.clone());

    // Create two notes in the project that we will "read" in the session
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

    // Build messages that read both notes (by permalink/title)
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

    // Run structural extraction
    let taxonomy = crate::actors::slot::session_extraction::run_structural_extraction(
        fixture.session_id.clone(),
        messages,
        ctx,
    )
    .await;

    // Taxonomy should be returned (not None)
    assert!(
        taxonomy.is_some(),
        "run_structural_extraction should return Some(taxonomy)"
    );
    let taxonomy = taxonomy.unwrap();
    assert_eq!(taxonomy.notes_read, 2);

    // Co-access association should now exist
    let associations = note_repo
        .get_associations_for_note(&note_a.id)
        .await
        .expect("get associations");
    assert!(
        !associations.is_empty(),
        "co-access association should have been flushed for note_a"
    );
    let assoc = &associations[0];
    let other_id = if assoc.note_a_id == note_a.id {
        &assoc.note_b_id
    } else {
        &assoc.note_a_id
    };
    assert_eq!(
        other_id, &note_b.id,
        "association should link note_a and note_b"
    );
}

// ─── AC3: LLM extraction with FakeProvider ────────────────────────────────────

/// AC3: FakeProvider produces case/pattern/pitfall notes in the DB.
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
    };

    let provider = fake_extraction_provider();
    run_llm_extraction_with_provider(fixture.session_id.clone(), taxonomy, ctx, provider).await;

    // Verify notes were written to DB
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

    assert_eq!(cases.len(), 1, "should have created 1 case note");
    assert_eq!(patterns.len(), 1, "should have created 1 pattern note");
    assert_eq!(pitfalls.len(), 1, "should have created 1 pitfall note");

    assert_eq!(cases[0].title, "Test Case Note");
    assert_eq!(patterns[0].title, "Test Pattern Note");
    assert_eq!(pitfalls[0].title, "Test Pitfall Note");
}

// ─── AC4: Confidence 0.5 and session provenance ───────────────────────────────

/// AC4 part 1: extracted notes have confidence exactly 0.5.
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
    };

    let provider = fake_extraction_provider();
    run_llm_extraction_with_provider(fixture.session_id.clone(), taxonomy, ctx, provider).await;

    let note_repo = NoteRepository::new(fixture.db.clone(), djinn_core::events::EventBus::noop());
    let all_notes = note_repo
        .list(&fixture.project.id, None)
        .await
        .expect("list notes");

    assert!(!all_notes.is_empty(), "should have written notes");
    for note in &all_notes {
        assert!(
            (note.confidence - 0.5).abs() < 1e-9,
            "note '{}' should have confidence 0.5, got {}",
            note.title,
            note.confidence
        );
    }
}

/// AC4 part 2: note content contains the session_id as provenance.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_extracted_notes_contain_session_id_provenance() {
    let fixture = make_fixture().await;
    let session_id = fixture.session_id.clone();
    let ctx = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());

    let taxonomy = SessionTaxonomy::default();

    // Use a custom provider that always returns exactly one case note
    let json = r#"{"cases":[{"title":"Provenance Test","content":"This content should have provenance."}],"patterns":[],"pitfalls":[]}"#;
    let provider = Arc::new(FakeProvider::text(json));

    run_llm_extraction_with_provider(session_id.clone(), taxonomy, ctx, provider).await;

    let note_repo = NoteRepository::new(fixture.db.clone(), djinn_core::events::EventBus::noop());
    let notes = note_repo
        .list(&fixture.project.id, None)
        .await
        .expect("list notes");

    assert_eq!(notes.len(), 1, "should have written exactly 1 note");
    let note = &notes[0];
    assert!(
        note.content.contains(&session_id),
        "note content should contain session_id '{}', got: {}",
        session_id,
        note.content
    );
    assert!(
        note.content.contains("Extracted from session"),
        "note content should contain provenance marker"
    );
    assert!(
        note.content.contains("0.5"),
        "note content should mention confidence 0.5 in provenance"
    );
}

// ─── AC5: Graceful degradation when LLM unavailable ──────────────────────────

/// AC5: When LLM is unavailable (FailingProvider), no notes are written and
/// the function returns without panicking or propagating errors.
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
    };

    // FailingProvider always returns an error from stream()
    let provider = Arc::new(FailingProvider::new("injected LLM failure for test"));
    // Should complete without panicking
    run_llm_extraction_with_provider(fixture.session_id.clone(), taxonomy, ctx, provider).await;

    // No notes should have been written
    let note_repo = NoteRepository::new(fixture.db.clone(), djinn_core::events::EventBus::noop());
    let notes = note_repo
        .list(&fixture.project.id, None)
        .await
        .expect("list notes");

    assert!(
        notes.is_empty(),
        "no notes should be written when LLM provider fails"
    );
}

/// AC5: When no credentials are configured (real code path), resolve_memory_provider
/// fails gracefully and no notes are written.
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
    };

    // No credentials configured — resolve_memory_provider will fail → graceful skip
    run_llm_extraction(fixture.session_id.clone(), taxonomy, ctx).await;

    let note_repo = NoteRepository::new(fixture.db.clone(), djinn_core::events::EventBus::noop());
    let notes = note_repo
        .list(&fixture.project.id, None)
        .await
        .expect("list notes");

    assert!(
        notes.is_empty(),
        "no notes should be written when no provider is configured"
    );
}

// ─── AC6: Dedup pipeline ──────────────────────────────────────────────────────

/// AC6: Running LLM extraction twice with the same title does not create
/// duplicate notes (the DB UNIQUE constraint on permalink enforces this,
/// meaning the second write fails gracefully and is skipped).
///
/// Note: The current implementation skips note creation when the permalink
/// already exists (the DB insert fails). We verify the count stays at 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_extraction_repeated_sessions_produce_no_duplicate_notes() {
    let fixture = make_fixture().await;

    let taxonomy = SessionTaxonomy {
        files_changed: 2,
        errors: 1,
        git_ops: 1,
        tools_used: 4,
        notes_read: 0,
        notes_written: 1,
        tasks_transitioned: 1,
    };

    // Both sessions return the same title → same permalink → second insert fails.
    // The FakeProvider must be created fresh for each call (scripted turns = 1).
    for _ in 0..2_u32 {
        let ctx = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());
        let json = r#"{"cases":[{"title":"Duplicate Note Title","content":"Content for dedup test."}],"patterns":[],"pitfalls":[]}"#;
        let provider = Arc::new(FakeProvider::text(json));
        run_llm_extraction_with_provider(
            fixture.session_id.clone(),
            taxonomy.clone(),
            ctx,
            provider,
        )
        .await;
    }

    let note_repo = NoteRepository::new(fixture.db.clone(), djinn_core::events::EventBus::noop());
    let notes = note_repo
        .list(&fixture.project.id, None)
        .await
        .expect("list notes");

    // Only 1 note should exist despite running extraction twice
    let dedup_notes: Vec<_> = notes
        .iter()
        .filter(|n| n.title == "Duplicate Note Title")
        .collect();
    assert_eq!(
        dedup_notes.len(),
        1,
        "duplicate note should not be created on repeated extraction runs"
    );
}

// ─── AC1: Full pipeline integration ──────────────────────────────────────────

/// AC1: End-to-end test — structural extraction chained with LLM extraction
/// (using FakeProvider) writes notes to the DB correctly, verifying the
/// complete pipeline flow described in the task background.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_reflection_pipeline_structural_then_llm_extraction() {
    let fixture = make_fixture().await;
    let ctx_structural = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());
    let ctx_llm = agent_context_from_db(fixture.db.clone(), fixture.cancel.clone());

    // Build a session with some tool calls (no actual notes to co-access here,
    // but the structural extraction should still return a taxonomy).
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

    // Step 1: structural extraction
    let taxonomy = crate::actors::slot::session_extraction::run_structural_extraction(
        fixture.session_id.clone(),
        messages,
        ctx_structural,
    )
    .await;

    assert!(
        taxonomy.is_some(),
        "structural extraction should return Some(taxonomy) for non-empty messages"
    );
    let taxonomy = taxonomy.unwrap();
    assert_eq!(taxonomy.files_changed, 1, "1 file written");
    assert_eq!(taxonomy.git_ops, 1, "1 git op");
    assert_eq!(taxonomy.notes_written, 1, "1 memory_write");
    assert_eq!(taxonomy.tasks_transitioned, 1, "1 task transition");

    // Verify taxonomy was also stored on the session record (queried directly
    // since the SessionRecord model does not surface event_taxonomy as a field)
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

    assert!(
        stored_json.is_some(),
        "event_taxonomy should be stored on session record after structural extraction"
    );
    let stored_taxonomy: SessionTaxonomy =
        serde_json::from_str(stored_json.as_deref().unwrap()).expect("deserialize stored taxonomy");
    assert_eq!(stored_taxonomy.files_changed, 1);
    assert_eq!(stored_taxonomy.git_ops, 1);

    // Step 2: LLM extraction
    let provider = fake_extraction_provider();
    run_llm_extraction_with_provider(fixture.session_id.clone(), taxonomy, ctx_llm, provider).await;

    // Verify notes were written with correct types
    let note_repo = NoteRepository::new(fixture.db.clone(), djinn_core::events::EventBus::noop());
    let all_notes = note_repo
        .list(&fixture.project.id, None)
        .await
        .expect("list notes");

    assert!(!all_notes.is_empty(), "full pipeline should produce notes");

    let note_types: Vec<_> = all_notes.iter().map(|n| n.note_type.as_str()).collect();
    assert!(
        note_types.contains(&"case"),
        "pipeline should produce case notes"
    );
    assert!(
        note_types.contains(&"pattern"),
        "pipeline should produce pattern notes"
    );
    assert!(
        note_types.contains(&"pitfall"),
        "pipeline should produce pitfall notes"
    );

    // All notes should have confidence 0.5 and session provenance
    for note in &all_notes {
        assert!(
            (note.confidence - 0.5).abs() < 1e-9,
            "note '{}' should have confidence 0.5, got {}",
            note.title,
            note.confidence
        );
        assert!(
            note.content.contains(&fixture.session_id),
            "note '{}' should contain session_id in content",
            note.title
        );
    }
}
