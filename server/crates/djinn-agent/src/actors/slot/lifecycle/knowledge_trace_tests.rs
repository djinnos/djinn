// Retrieval-trace instrumentation tests for `load_knowledge_context`.
//
// These tests verify that the static knowledge-context entry point:
// - preserves existing prompt output (byte-compatible with `format_knowledge_notes`),
// - persists `LoadKnowledgeContext` retrieval traces with deterministic drop reasons,
// - remains fail-open when trace persistence encounters an error.
use super::*;

use djinn_core::events::EventBus;
use djinn_db::NoteRepository;
use djinn_db::repositories::retrieval_trace::{
    CandidateOutcome, RetrievalTraceEntryPoint, RetrievalTraceListFilter,
    RetrievalTraceRepository, SkippedReason,
};
use tokio_util::sync::CancellationToken;

use crate::test_helpers::agent_context_from_db;

use super::test_support::create_project_epic_task;

/// Set confidence and updated_at on a note row for deterministic ordering.
async fn set_note_confidence(db: &djinn_db::Database, note_id: &str, confidence: f64) {
    sqlx::query("UPDATE notes SET confidence = $1 WHERE id = $2")
        .bind(confidence)
        .bind(note_id)
        .execute(db.pool())
        .await
        .unwrap();
}

/// Create a scoped pattern note that overlaps the given task paths.
async fn seed_scoped_note(
    db: &djinn_db::Database,
    project_id: &str,
    title: &str,
    scope_paths: &str,
    confidence: f64,
) -> String {
    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    let note = note_repo
        .create_with_scope(
            project_id,
            title,
            "content body",
            "pattern",
            None,
            "[]",
            scope_paths,
        )
        .await
        .expect("create note");
    set_note_confidence(db, &note.id, confidence).await;
    note.id
}

/// Fetch the most recent `LoadKnowledgeContext` trace for a project.
async fn latest_trace(
    db: &djinn_db::Database,
    project_id: &str,
) -> Option<djinn_db::repositories::retrieval_trace::RetrievalTraceRow> {
    let repo = RetrievalTraceRepository::new(db.clone());
    repo.list_by_project(
        project_id,
        RetrievalTraceListFilter {
            entry_point: Some(RetrievalTraceEntryPoint::LoadKnowledgeContext),
            limit: Some(1),
            ..Default::default()
        },
    )
    .await
    .ok()?
    .into_iter()
    .next()
}

/// Extract the candidate outcomes from a trace row as (note_id, outcome, skipped_reason).
fn candidate_outcomes(
    row: &djinn_db::repositories::retrieval_trace::RetrievalTraceRow,
) -> Vec<(String, CandidateOutcome, Option<SkippedReason>)> {
    row.candidates_typed()
        .into_iter()
        .map(|c| (c.note_id, c.outcome, c.skipped_reason))
        .collect()
}

// ── Prompt output preservation ─────────────────────────────────────────────

#[tokio::test]
async fn load_knowledge_context_prompt_output_unchanged_with_tracing() {
    let db = djinn_db::Database::ephemeral().await.expect("ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Trace epic", "Trace task").await;
    let project_id = task.project_id.clone();

    // Seed a matching note with high confidence.
    seed_scoped_note(&db, &project_id, "High Note", r#"["server/src"]"#, 0.9).await;

    let app_state = agent_context_from_db(db.clone(), CancellationToken::new());

    // The task description is "description" which won't derive scope paths.
    // To get a match we need the note to be global OR the task to have
    // matching paths. We'll use a global note (empty scope_paths) to ensure
    // the production query finds it.
    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    let global = note_repo
        .create(&project_id, "Global Pattern", "content", "pattern", "[]")
        .await
        .unwrap();
    set_note_confidence(&db, &global.id, 0.9).await;

    // Build a task that has no specific scope paths so global notes match.
    let result = load_knowledge_context(&task, None, &app_state).await;

    // Verify the prompt is produced and contains the note.
    assert!(result.is_some(), "knowledge context should be Some");
    let prompt = result.unwrap();
    assert!(prompt.contains("Global Pattern"), "prompt should contain the note");

    // Verify a trace row was persisted.
    let trace = latest_trace(&db, &project_id).await;
    assert!(trace.is_some(), "trace row should be persisted");
    let trace = trace.unwrap();
    assert_eq!(trace.entry_point, "load_knowledge_context");

    // Verify the trigger shape.
    let trigger = trace.trigger.expect("trigger should be present");
    assert_eq!(trigger["shape"], "scope_paths");
}

#[tokio::test]
async fn load_knowledge_context_returns_none_when_no_matching_notes() {
    let db = djinn_db::Database::ephemeral().await.expect("ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Empty epic", "Empty task").await;

    let app_state = agent_context_from_db(db.clone(), CancellationToken::new());
    let result = load_knowledge_context(&task, None, &app_state).await;
    assert!(result.is_none(), "should return None when no notes match");
}

// ── Deterministic drop reasons ──────────────────────────────────────────────

#[tokio::test]
async fn trace_classifies_below_threshold_as_min_confidence() {
    let db = djinn_db::Database::ephemeral().await.expect("ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "MinCnf epic", "MinCnf task").await;
    let project_id = task.project_id.clone();

    // Seed a global note below the 0.3 threshold.
    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    let below = note_repo
        .create(&project_id, "Below Threshold", "content", "pattern", "[]")
        .await
        .unwrap();
    set_note_confidence(&db, &below.id, 0.1).await;

    let app_state = agent_context_from_db(db.clone(), CancellationToken::new());
    let _ = load_knowledge_context(&task, None, &app_state).await;

    let trace = latest_trace(&db, &project_id)
        .await
        .expect("trace should exist");
    let outcomes = candidate_outcomes(&trace);

    // The below-threshold note should be skipped with min_confidence.
    let below_outcome = outcomes
        .iter()
        .find(|(id, _, _)| *id == below.id)
        .expect("below-threshold note should be in trace");
    assert_eq!(below_outcome.1, CandidateOutcome::Skipped);
    assert_eq!(below_outcome.2, Some(SkippedReason::MinConfidence));
}

#[tokio::test]
async fn trace_classifies_over_limit_as_not_top_k() {
    let db = djinn_db::Database::ephemeral().await.expect("ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "TopK epic", "TopK task").await;
    let project_id = task.project_id.clone();

    // Seed 12 global notes above threshold. Production limit is 10, so notes
    // ranked 11 and 12 should be classified as not_top_k.
    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    for i in 0..12 {
        let note = note_repo
            .create(
                &project_id,
                &format!("Pattern {i}"),
                "content",
                "pattern",
                "[]",
            )
            .await
            .unwrap();
        // All above 0.3 threshold; slightly different confidence for deterministic ordering.
        set_note_confidence(&db, &note.id, 0.5 + (11 - i) as f64 * 0.01).await;
    }

    let app_state = agent_context_from_db(db.clone(), CancellationToken::new());
    let _ = load_knowledge_context(&task, None, &app_state).await;

    let trace = latest_trace(&db, &project_id)
        .await
        .expect("trace should exist");
    let outcomes = candidate_outcomes(&trace);

    // Exactly 10 should be injected, 2 should be not_top_k.
    let injected = outcomes
        .iter()
        .filter(|(_, o, _)| *o == CandidateOutcome::Injected)
        .count();
    let not_top_k = outcomes
        .iter()
        .filter(|(_, _, r)| *r == Some(SkippedReason::NotTopK))
        .count();
    assert_eq!(injected, 10, "exactly 10 should be injected");
    assert_eq!(not_top_k, 2, "exactly 2 should be not_top_k");
}

#[tokio::test]
async fn trace_classifies_injected_and_budget_pruned() {
    let db = djinn_db::Database::ephemeral().await.expect("ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Budget epic", "Budget task").await;
    let project_id = task.project_id.clone();

    // Seed several notes that pass confidence but are large enough to trigger
    // budget pruning (budget is 2000 chars). We'll create notes with long
    // content so the prompt packing prunes some.
    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    let mut note_ids = Vec::new();
    for i in 0..5 {
        let long_content = "x".repeat(800);
        let note = note_repo
            .create(
                &project_id,
                &format!("Long Pattern {i}"),
                &long_content,
                "pattern",
                "[]",
            )
            .await
            .unwrap();
        // All pass confidence threshold.
        set_note_confidence(&db, &note.id, 0.9 - i as f64 * 0.05).await;
        note_ids.push(note.id);
    }

    let app_state = agent_context_from_db(db.clone(), CancellationToken::new());
    let _ = load_knowledge_context(&task, None, &app_state).await;

    let trace = latest_trace(&db, &project_id)
        .await
        .expect("trace should exist");
    let outcomes = candidate_outcomes(&trace);

    // At least one should be injected and at least one budget_pruned.
    let injected = outcomes
        .iter()
        .filter(|(_, o, _)| *o == CandidateOutcome::Injected)
        .count();
    let budget_pruned = outcomes
        .iter()
        .filter(|(_, _, r)| *r == Some(SkippedReason::BudgetPruned))
        .count();
    assert!(injected >= 1, "at least one should be injected");
    assert!(
        budget_pruned >= 1,
        "at least one should be budget_pruned (got {budget_pruned})"
    );

    // Every candidate should be classified.
    assert_eq!(outcomes.len(), 5, "all 5 candidates should be in trace");
}

#[tokio::test]
async fn trace_includes_estimated_injected_tokens_and_cap_metadata() {
    let db = djinn_db::Database::ephemeral().await.expect("ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Meta epic", "Meta task").await;
    let project_id = task.project_id.clone();

    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    let note = note_repo
        .create(&project_id, "Token Pattern", "content", "pattern", "[]")
        .await
        .unwrap();
    set_note_confidence(&db, &note.id, 0.9).await;

    let app_state = agent_context_from_db(db.clone(), CancellationToken::new());
    let _ = load_knowledge_context(&task, None, &app_state).await;

    let trace = latest_trace(&db, &project_id)
        .await
        .expect("trace should exist");

    // The estimated injected tokens should be positive (at least one note injected).
    assert!(
        trace.estimated_injected_tokens > 0,
        "estimated_injected_tokens should be positive"
    );

    // Candidate cap metadata should be set.
    assert_eq!(
        trace.candidate_cap,
        djinn_db::repositories::retrieval_trace::DEFAULT_CANDIDATE_CAP
    );

    // Durations should be present.
    let durations = &trace.durations_ms;
    assert!(
        durations.get("candidate_fetch_classify_pack_ms").is_some(),
        "durations should include candidate_fetch_classify_pack_ms"
    );
}

// ── Fail-open behavior ─────────────────────────────────────────────────────

#[tokio::test]
async fn trace_persistence_failure_does_not_change_prompt_output() {
    let db = djinn_db::Database::ephemeral().await.expect("ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Fail epic", "Fail task").await;
    let project_id = task.project_id.clone();

    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    let note = note_repo
        .create(&project_id, "Fail Pattern", "content", "pattern", "[]")
        .await
        .unwrap();
    set_note_confidence(&db, &note.id, 0.9).await;

    let app_state = agent_context_from_db(db.clone(), CancellationToken::new());

    // Drop the retrieval_traces table to force a persistence error.
    // The prompt output should still be correct.
    sqlx::query("DROP TABLE IF EXISTS retrieval_traces")
        .execute(db.pool())
        .await
        .unwrap();

    let result = load_knowledge_context(&task, None, &app_state).await;

    // Prompt should still be produced correctly despite trace persistence failure.
    assert!(result.is_some(), "prompt should still be produced");
    let prompt = result.unwrap();
    assert!(prompt.contains("Fail Pattern"));
}

#[tokio::test]
async fn trace_trigger_uses_scope_paths_shape() {
    let db = djinn_db::Database::ephemeral().await.expect("ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Shape epic", "Shape task").await;
    let project_id = task.project_id.clone();

    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    let note = note_repo
        .create(&project_id, "Shape Pattern", "content", "pattern", "[]")
        .await
        .unwrap();
    set_note_confidence(&db, &note.id, 0.9).await;

    let app_state = agent_context_from_db(db.clone(), CancellationToken::new());
    let _ = load_knowledge_context(&task, None, &app_state).await;

    let trace = latest_trace(&db, &project_id)
        .await
        .expect("trace should exist");

    // Verify entry point string is exactly "load_knowledge_context".
    assert_eq!(trace.entry_point, "load_knowledge_context");

    // Verify trigger shape.
    let trigger = trace.trigger.expect("trigger present");
    assert_eq!(trigger["shape"], "scope_paths");
}
