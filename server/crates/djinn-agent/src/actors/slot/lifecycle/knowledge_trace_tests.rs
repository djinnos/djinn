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
    CandidateOutcome, RetrievalTraceEntryPoint, RetrievalTraceListFilter, RetrievalTraceOutcome,
    RetrievalTraceRepository, SkippedReason,
};
use tokio_util::sync::CancellationToken;

use crate::test_helpers::agent_context_from_db;

use super::test_support::create_project_epic_task;

/// Set confidence and updated_at on a note row for deterministic ordering.
async fn set_note_confidence(db: &djinn_db::Database, note_id: &str, confidence: f64) {
    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    note_repo
        .set_confidence(note_id, confidence)
        .await
        .expect("set_confidence");
}

/// Create a scoped pattern note that overlaps the given task paths.
///
/// R1: returns the whole note rather than only its id, because the rendered
/// prompt line is now labelled by the note's `permalink` (the title is no
/// longer rendered at all). Callers that need to assert "this note reached the
/// prompt" must look for `note.permalink`, taken from the created row instead
/// of a retyped slug literal.
async fn seed_scoped_note(
    db: &djinn_db::Database,
    task: &Task,
    title: &str,
    scope_paths: &str,
    confidence: f64,
) -> djinn_memory::Note {
    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    let note = note_repo
        .create_with_scope(
            &task.project_id,
            title,
            &related_content(task, "content body"),
            "pattern",
            None,
            "[]",
            scope_paths,
        )
        .await
        .expect("create note");
    set_note_confidence(db, &note.id, confidence).await;
    note
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

fn assert_exceptional_terminal(
    trace: &djinn_db::repositories::retrieval_trace::RetrievalTraceRow,
    outcome: RetrievalTraceOutcome,
) {
    assert_eq!(trace.knowledge_trace_taxonomy_version, Some(1));
    assert_eq!(trace.terminal_state.as_deref(), Some("error"));
    assert!(
        trace.terminal_at.is_some(),
        "exceptional terminal has timestamp"
    );
    assert_eq!(trace.outcome, outcome);
    assert_eq!(trace.candidate_count, None);
    assert_eq!(trace.injected_count, None);
    assert_eq!(trace.confidence_filtered_count, None);
    assert_eq!(trace.not_top_k_count, None);
    assert_eq!(trace.oversized_skipped_count, None);
    assert_eq!(trace.budget_pruned_count, None);
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
    let mut env = knowledge_context_test_env_guard();
    env.clear();
    let db = djinn_db::Database::ephemeral().await.expect("ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Trace epic", "Trace task").await;
    let project_id = task.project_id.clone();

    let app_state = agent_context_from_db(db.clone(), CancellationToken::new());

    // Under ranked retrieval a note must be *about the task* to be retrieved,
    // so this one carries the task's own text. (The retired scope-overlap query
    // returned every global note regardless of content.)
    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    let global = note_repo
        .create(
            &project_id,
            "Global Pattern",
            &related_content(&task, "global pattern body"),
            "pattern",
            "[]",
        )
        .await
        .unwrap();
    set_note_confidence(&db, &global.id, 0.9).await;

    let result = load_knowledge_context(&task, None, &app_state, None).await;

    // Verify the prompt is produced and contains the note.
    assert!(result.is_some(), "knowledge context should be Some");
    let prompt = result.unwrap();
    // R1: the note's title is no longer rendered; the permalink is the line's
    // label. Same property ("this note reached the prompt"), identified by the
    // permalink taken from the created row.
    assert!(
        prompt.contains(&global.permalink),
        "prompt should contain the note"
    );

    // Verify a trace row was persisted.
    let trace = latest_trace(&db, &project_id).await;
    assert!(trace.is_some(), "trace row should be persisted");
    let trace = trace.unwrap();
    assert_eq!(trace.entry_point, "load_knowledge_context");

    // Verify the trigger shape.
    let trigger = trace.trigger.expect("trigger should be present");
    assert_eq!(trigger["shape"], "ranked_injection_v1");
}

/// AC2: candidates come **only** from the ranked RRF search.
///
/// The retired `query_by_scope_overlap` returned every global note above the
/// confidence floor regardless of whether it had anything to do with the task —
/// that is precisely the recency lottery proposal `5205` removes. This test
/// pins the side effect: an unrelated global note is no longer injected, and is
/// absent from the candidate universe entirely, while a related one is
/// injected. It is also verified against the retired query directly, so the
/// test cannot pass by the note simply not existing.
#[tokio::test]
async fn unrelated_global_note_is_no_longer_a_candidate() {
    let mut env = knowledge_context_test_env_guard();
    env.clear();
    let db = djinn_db::Database::ephemeral().await.expect("ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Relevance epic", "Relevance task").await;
    let project_id = task.project_id.clone();
    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());

    let related = note_repo
        .create(
            &project_id,
            "Related Pattern",
            &related_content(&task, "related body"),
            "pattern",
            "[]",
        )
        .await
        .unwrap();
    set_note_confidence(&db, &related.id, 0.9).await;

    // Shares no term with the task and carries no scope path.
    let unrelated = note_repo
        .create(
            &project_id,
            "Quokka Ledger Reconciliation",
            "quokka ledger reconciliation body",
            "pattern",
            "[]",
        )
        .await
        .unwrap();
    set_note_confidence(&db, &unrelated.id, 0.99).await;

    // Control: the retired query *would* have returned the unrelated note — and
    // ahead of the related one, since it orders by `confidence DESC`. Without
    // this the negative assertion below could pass vacuously.
    let legacy = note_repo
        .query_by_scope_overlap(
            &project_id,
            &derive_task_scope_path_tokens(&task, None),
            KNOWLEDGE_NOTE_TYPES,
            KNOWLEDGE_MIN_CONFIDENCE,
            10,
        )
        .await
        .expect("legacy query");
    assert_eq!(
        legacy.first().map(|note| note.id.as_str()),
        Some(unrelated.id.as_str()),
        "the retired query ranked the unrelated note first, by confidence"
    );

    let app_state = agent_context_from_db(db.clone(), CancellationToken::new());
    let rendered = load_knowledge_context(&task, None, &app_state, None)
        .await
        .expect("the related note is injected");

    assert!(
        rendered.contains(&related.permalink),
        "the task-relevant note must be injected"
    );
    assert!(
        !rendered.contains(&unrelated.permalink),
        "an unrelated global note must no longer reach the prompt"
    );

    let trace = latest_trace(&db, &project_id)
        .await
        .expect("trace should exist");
    let candidate_ids: Vec<String> = trace
        .candidates_typed()
        .into_iter()
        .map(|candidate| candidate.note_id)
        .collect();
    assert!(
        candidate_ids.contains(&related.id),
        "the related note is a candidate"
    );
    assert!(
        !candidate_ids.contains(&unrelated.id),
        "the unrelated note must not even enter the candidate universe"
    );
}

#[tokio::test]
async fn load_knowledge_context_returns_none_when_no_matching_notes() {
    let mut env = knowledge_context_test_env_guard();
    env.clear();
    let db = djinn_db::Database::ephemeral().await.expect("ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Empty epic", "Empty task").await;

    let app_state = agent_context_from_db(db.clone(), CancellationToken::new());
    let result = load_knowledge_context(&task, None, &app_state, None).await;
    assert!(result.is_none(), "should return None when no notes match");
}

// ── Deterministic drop reasons ──────────────────────────────────────────────

#[tokio::test]
async fn trace_classifies_below_threshold_as_min_confidence() {
    let mut env = knowledge_context_test_env_guard();
    env.clear();
    let db = djinn_db::Database::ephemeral().await.expect("ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "MinCnf epic", "MinCnf task").await;
    let project_id = task.project_id.clone();

    // A note below the 0.3 threshold. Retrieval has no confidence filter — the
    // floor is applied by packing — so it still reaches the trace, where it
    // must be dispositioned `min_confidence`.
    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    let below = note_repo
        .create(
            &project_id,
            "Below Threshold",
            &related_content(&task, "below threshold body"),
            "pattern",
            "[]",
        )
        .await
        .unwrap();
    set_note_confidence(&db, &below.id, 0.1).await;

    let app_state = agent_context_from_db(db.clone(), CancellationToken::new());
    let _ = load_knowledge_context(&task, None, &app_state, None).await;

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
    let mut env = knowledge_context_test_env_guard();
    env.clear();
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
                &related_content(&task, &format!("pattern body {i}")),
                "pattern",
                "[]",
            )
            .await
            .unwrap();
        // All above the 0.3 threshold. Which two land outside top-K is now
        // decided by relevance ranking rather than confidence order, so this
        // test asserts the counts, not the identities.
        set_note_confidence(&db, &note.id, 0.5 + (11 - i) as f64 * 0.01).await;
    }

    let app_state = agent_context_from_db(db.clone(), CancellationToken::new());
    let _ = load_knowledge_context(&task, None, &app_state, None).await;

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
    let mut env = knowledge_context_test_env_guard();
    env.clear();
    let db = djinn_db::Database::ephemeral().await.expect("ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Budget epic", "Budget task").await;
    let project_id = task.project_id.clone();

    // Seed 10 notes all with high confidence (> 0.8) so each summary uses
    // 200 chars from content. With ~270 chars per rendered line, 10 notes
    // total ~2700 chars which exceeds the 2000-char budget, causing the
    // last few notes to be budget-pruned by `pack_knowledge_notes`.
    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    let mut note_ids = Vec::new();
    for i in 0..10 {
        let long_content = related_content(&task, &"x".repeat(800));
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
        // All high confidence (> 0.8) so each summary uses 200 chars.
        set_note_confidence(&db, &note.id, 0.95 - i as f64 * 0.01).await;
        note_ids.push(note.id);
    }

    let app_state = agent_context_from_db(db.clone(), CancellationToken::new());
    let _ = load_knowledge_context(&task, None, &app_state, None).await;

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
    assert_eq!(outcomes.len(), 10, "all 10 candidates should be in trace");
}

#[tokio::test]
async fn trace_includes_estimated_injected_tokens_and_cap_metadata() {
    let mut env = knowledge_context_test_env_guard();
    env.clear();
    let db = djinn_db::Database::ephemeral().await.expect("ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Meta epic", "Meta task").await;
    let project_id = task.project_id.clone();

    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    let note = note_repo
        .create(
            &project_id,
            "Token Pattern",
            &related_content(&task, "token pattern body"),
            "pattern",
            "[]",
        )
        .await
        .unwrap();
    set_note_confidence(&db, &note.id, 0.9).await;

    let app_state = agent_context_from_db(db.clone(), CancellationToken::new());
    let _ = load_knowledge_context(&task, None, &app_state, None).await;

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

    // Durations should be present with separate per-phase keys.
    let durations = &trace.durations_ms;
    assert!(
        durations.get("candidate_fetch_ms").is_some(),
        "durations should include candidate_fetch_ms"
    );
    assert!(
        durations.get("classify_ms").is_some(),
        "durations should include classify_ms"
    );
    assert!(
        durations.get("prompt_pack_ms").is_some(),
        "durations should include prompt_pack_ms"
    );
    assert!(
        durations.get("persist_ms").is_some(),
        "durations should include persist_ms"
    );
}

// ── Fail-open behavior ─────────────────────────────────────────────────────

#[tokio::test]
async fn trace_persistence_failure_does_not_change_prompt_output() {
    let mut env = knowledge_context_test_env_guard();
    env.clear();
    let db = djinn_db::Database::ephemeral().await.expect("ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Fail epic", "Fail task").await;
    let project_id = task.project_id.clone();

    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    let note = note_repo
        .create(
            &project_id,
            "Fail Pattern",
            &related_content(&task, "fail pattern body"),
            "pattern",
            "[]",
        )
        .await
        .unwrap();
    set_note_confidence(&db, &note.id, 0.9).await;

    let app_state = agent_context_from_db(db.clone(), CancellationToken::new());

    // Capture the normal, tracing-success rendered output first. This is the
    // production baseline that must be preserved even when trace persistence
    // fails. The same code path produces the same packed string from the same
    // production note set, so a byte-for-byte comparison is the right regression
    // guard against accidental injection of extra trace-related text.
    let expected = load_knowledge_context(&task, None, &app_state, None)
        .await
        .expect("baseline prompt should be produced");

    // Drop the retrieval_traces table to force a persistence error.
    // The prompt output should still be byte-identical to the baseline.
    djinn_db::test_support::drop_table_for_test(&db, "retrieval_traces").await;

    let result = load_knowledge_context(&task, None, &app_state, None).await;

    // Prompt should still be produced correctly despite trace persistence failure.
    assert!(result.is_some(), "prompt should still be produced");
    let prompt = result.unwrap();
    assert_eq!(
        prompt, expected,
        "prompt output must be byte-identical with and without trace persistence"
    );
    // R1: the seeded note is identified by its permalink label, not its title.
    assert!(prompt.contains(&note.permalink));
}

/// AC10: retrieval traces expose the strategy, the ranking profile, the
/// validated scope (or its typed fallback reason), the per-signal ranks, the
/// fused rank/score, and exactly one terminal disposition per candidate.
#[tokio::test]
async fn trace_exposes_strategy_profile_scope_and_per_signal_ranks() {
    let mut env = knowledge_context_test_env_guard();
    env.clear();
    let db = djinn_db::Database::ephemeral().await.expect("ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Shape epic", "Shape task").await;
    let project_id = task.project_id.clone();

    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    let note = note_repo
        .create(
            &project_id,
            "Shape Pattern",
            &related_content(&task, "shape pattern body"),
            "pattern",
            "[]",
        )
        .await
        .unwrap();
    set_note_confidence(&db, &note.id, 0.9).await;

    let app_state = agent_context_from_db(db.clone(), CancellationToken::new());
    let _ = load_knowledge_context(&task, None, &app_state, None).await;

    let trace = latest_trace(&db, &project_id)
        .await
        .expect("trace should exist");
    assert_eq!(trace.entry_point, "load_knowledge_context");

    let trigger = trace.trigger.clone().expect("trigger present");
    assert_eq!(trigger["shape"], "ranked_injection_v1");
    assert_eq!(trigger["strategy"], "ranked_injection_v1");
    assert_eq!(trigger["ranking_profile"], "knowledge_injection_v1");
    assert_eq!(trigger["candidate_window"], 50);
    // top_k defaults to 10, so rrf_k = clamp(20 + 2*10, 30, 60) = 40.
    assert_eq!(trigger["rrf_k"], 40.0);
    // No base tree was supplied, so scope derivation reports the typed reason
    // rather than silently returning unvalidated regex tokens.
    assert_eq!(
        trigger["scope_fallback_reason"], "tree_provider_unavailable",
        "provider unavailability must be distinguishable from an empty match"
    );
    assert_eq!(
        trigger["task_paths"],
        serde_json::json!([]),
        "an unavailable provider yields no scope paths at all"
    );
    assert!(
        trigger["search_error"].is_null(),
        "a successful retrieval records no search_error"
    );

    // Per-candidate provenance: the fused rank/score and every signal's rank.
    let candidates = trace.candidates_typed();
    assert!(!candidates.is_empty(), "the seeded note must be retrieved");
    let seeded = candidates
        .iter()
        .find(|candidate| candidate.note_id == note.id)
        .expect("seeded note present in trace");
    assert_eq!(seeded.rank, Some(1), "fused rank is 1-based");
    let scope = seeded.scope.as_ref().expect("scope payload present");
    assert_eq!(scope["ranking_profile"], "knowledge_injection_v1");
    assert_eq!(scope["fused_rank"], 1);
    assert!(
        scope["fused_score"]
            .as_f64()
            .is_some_and(|score| score > 0.0),
        "fused score must be recorded, got {scope}"
    );
    let signal_ranks = &scope["signal_ranks"];
    assert!(
        signal_ranks.is_object(),
        "per-signal ranks must be recorded, got {scope}"
    );
    for signal in [
        "lexical",
        "semantic",
        "temporal",
        "graph",
        "task_affinity",
        "scope",
    ] {
        assert!(
            signal_ranks.get(signal).is_some(),
            "signal `{signal}` missing from {signal_ranks}"
        );
    }
    assert_eq!(
        signal_ranks["lexical"], 1,
        "the note was found by the lexical signal"
    );
    assert!(
        signal_ranks["scope"].is_null(),
        "no validated scope path, so the note is not in the scope signal"
    );

    // Exactly one terminal disposition per candidate.
    let dispositions = trace.confidence_filtered_count.unwrap_or(0)
        + trace.not_top_k_count.unwrap_or(0)
        + trace.oversized_skipped_count.unwrap_or(0)
        + trace.injected_count.unwrap_or(0)
        + trace.budget_pruned_count.unwrap_or(0);
    assert_eq!(
        dispositions,
        trace.candidate_count.unwrap_or(0),
        "the disposition histogram must partition the candidate set"
    );
}

// ── Pure classification unit tests (no database required) ────────────────────
//
// These exercise the deterministic drop-reason classification helpers directly,
// covering `dedupe` and `superseded_pruned` paths that are hard to trigger via
// seeded database rows alone.

use djinn_db::repositories::note::ScopeOverlapTraceCandidate;

/// Build a `ScopeOverlapTraceCandidate` for testing.
fn tc(
    id: &str,
    permalink: &str,
    title: &str,
    confidence: f64,
    rank: i64,
) -> ScopeOverlapTraceCandidate {
    ScopeOverlapTraceCandidate {
        id: id.to_string(),
        permalink: permalink.to_string(),
        title: title.to_string(),
        folder: "patterns".to_string(),
        note_type: "pattern".to_string(),
        scope_paths: "[]".to_string(),
        confidence,
        rank,
    }
}

#[test]
fn classify_below_threshold_is_min_confidence() {
    let candidates = vec![tc("n1", "p/n1", "Note 1", 0.1, 1)];
    let production_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let result = classify_knowledge_candidates(&candidates, &production_ids);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].outcome, CandidateOutcome::Skipped);
    assert_eq!(result[0].skipped_reason, Some(SkippedReason::MinConfidence));
}

#[test]
fn classify_above_threshold_not_in_production_is_not_top_k() {
    let candidates = vec![tc("n1", "p/n1", "Note 1", 0.5, 1)];
    let production_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let result = classify_knowledge_candidates(&candidates, &production_ids);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].outcome, CandidateOutcome::Skipped);
    assert_eq!(result[0].skipped_reason, Some(SkippedReason::NotTopK));
}

#[test]
fn classify_in_production_set_is_injected() {
    let candidates = vec![tc("n1", "p/n1", "Note 1", 0.9, 1)];
    let production_ids: std::collections::HashSet<&str> = std::collections::HashSet::from(["n1"]);
    let result = classify_knowledge_candidates(&candidates, &production_ids);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].outcome, CandidateOutcome::Injected);
    assert_eq!(result[0].skipped_reason, None);
}

#[test]
fn apply_budget_outcomes_dedupe_duplicate_permalink() {
    // Two candidates with the same permalink that are both in the production set
    // — the second should be dedupe'd.
    let candidates_raw = vec![
        tc("n1", "p/dup", "Dup 1", 0.9, 1),
        tc("n2", "p/dup", "Dup 2", 0.8, 2),
    ];
    let production_ids: std::collections::HashSet<&str> =
        std::collections::HashSet::from(["n1", "n2"]);
    let classified = classify_knowledge_candidates(&candidates_raw, &production_ids);

    // Build a packed result where both notes are injected (within budget).
    let packed = crate::actors::slot::helpers::PackedKnowledgeNotes {
        rendered: String::new(),
        outcomes: vec![crate::actors::slot::helpers::NotePackOutcome {
            permalink: "p/dup".to_string(),
            title: "Dup 1".to_string(),
            disposition: crate::actors::slot::helpers::NotePackDisposition::Injected,
            estimated_rendered_chars: Some(100),
            estimated_rendered_tokens: Some(25),
            action_excerpt: None,
        }],
        total_injected_chars: 100,
        total_injected_tokens: 25,
    };

    let notes: Vec<djinn_memory::Note> = Vec::new();
    let result = apply_budget_outcomes(classified, &packed, &notes);

    // First should be injected, second should be dedupe.
    let injected = result
        .iter()
        .filter(|c| c.outcome == CandidateOutcome::Injected)
        .count();
    let deduped = result
        .iter()
        .filter(|c| c.skipped_reason == Some(SkippedReason::Dedupe))
        .count();
    assert_eq!(injected, 1, "one should remain injected");
    assert_eq!(deduped, 1, "one should be dedupe'd");
}

#[test]
fn apply_budget_outcomes_budget_pruned_injected_candidate() {
    // A candidate in the production set whose packed disposition is BudgetPruned.
    let candidates_raw = vec![tc("n1", "p/n1", "Note 1", 0.9, 1)];
    let production_ids: std::collections::HashSet<&str> = std::collections::HashSet::from(["n1"]);
    let classified = classify_knowledge_candidates(&candidates_raw, &production_ids);

    let packed = crate::actors::slot::helpers::PackedKnowledgeNotes {
        rendered: String::new(),
        outcomes: vec![crate::actors::slot::helpers::NotePackOutcome {
            permalink: "p/n1".to_string(),
            title: "Note 1".to_string(),
            disposition: crate::actors::slot::helpers::NotePackDisposition::BudgetPruned,
            estimated_rendered_chars: None,
            estimated_rendered_tokens: None,
            action_excerpt: None,
        }],
        total_injected_chars: 0,
        total_injected_tokens: 0,
    };

    let notes: Vec<djinn_memory::Note> = Vec::new();
    let result = apply_budget_outcomes(classified, &packed, &notes);

    assert_eq!(result.len(), 1);
    assert_eq!(result[0].outcome, CandidateOutcome::Skipped);
    assert_eq!(result[0].skipped_reason, Some(SkippedReason::BudgetPruned));
}

/// Regression: an `OversizedSkipped` pack disposition used to leave the
/// candidate `Injected`, because only `BudgetPruned` was matched. The note was
/// dropped from the prompt while the trace claimed it had been injected.
#[test]
fn apply_budget_outcomes_oversized_skipped_injected_candidate() {
    let candidates_raw = vec![tc("n1", "p/n1", "Note 1", 0.9, 1)];
    let production_ids: std::collections::HashSet<&str> = std::collections::HashSet::from(["n1"]);
    let classified = classify_knowledge_candidates(&candidates_raw, &production_ids);
    assert_eq!(
        classified[0].outcome,
        CandidateOutcome::Injected,
        "precondition: the candidate starts out injected"
    );

    let packed = crate::actors::slot::helpers::PackedKnowledgeNotes {
        rendered: String::new(),
        outcomes: vec![crate::actors::slot::helpers::NotePackOutcome {
            permalink: "p/n1".to_string(),
            title: "Note 1".to_string(),
            disposition: crate::actors::slot::helpers::NotePackDisposition::OversizedSkipped,
            estimated_rendered_chars: None,
            estimated_rendered_tokens: None,
            action_excerpt: None,
        }],
        total_injected_chars: 0,
        total_injected_tokens: 0,
    };

    let notes: Vec<djinn_memory::Note> = Vec::new();
    let result = apply_budget_outcomes(classified, &packed, &notes);

    assert_eq!(result.len(), 1);
    assert_eq!(result[0].outcome, CandidateOutcome::Skipped);
    assert_eq!(
        result[0].skipped_reason,
        Some(SkippedReason::OversizedSkipped),
        "an oversized drop must not be reported as budget_pruned"
    );
}

// ── Oversized-vs-budget disposition reporting (proposal u46i AC4) ────────────

/// Build a note whose rendered summary line costs `permalink_len` bytes of
/// fixed overhead in the permalink alone, so `line_byte_cap` can be tuned to
/// force `rendered_line` to return `None` (a whole-note DROP).
fn oversize_test_note(id: &str, permalink: &str, confidence: f64) -> djinn_memory::Note {
    djinn_memory::Note {
        id: id.to_string(),
        project_id: "p".into(),
        permalink: permalink.to_string(),
        title: id.to_string(),
        file_path: String::new(),
        storage: "db".into(),
        note_type: "pattern".into(),
        folder: "patterns".into(),
        status: "active".into(),
        tags: "[]".into(),
        content: "body".into(),
        retrieval_anchor: None,
        created_at: String::new(),
        updated_at: String::new(),
        lifecycle_changed_at: None,
        last_accessed: String::new(),
        access_count: 0,
        confidence,
        abstract_: Some("abstract".to_string()),
        overview: None,
        scope_paths: "[]".into(),
    }
}

/// `trace_candidates_from_pack` must report the two non-injection dispositions
/// distinctly, and the distinction must survive serialization — the persisted
/// JSON is what the operator reads back through `memory_recall_trace`.
#[test]
fn trace_candidates_from_pack_reports_oversized_and_budget_distinctly() {
    use crate::actors::slot::helpers::{NotePackDisposition, NotePackOutcome};

    let notes = vec![
        oversize_test_note("n-oversized", "patterns/oversized", 0.9),
        oversize_test_note("n-budget", "patterns/budget", 0.8),
    ];
    let packed = crate::actors::slot::helpers::PackedKnowledgeNotes {
        rendered: String::new(),
        outcomes: vec![
            NotePackOutcome {
                permalink: "patterns/oversized".to_string(),
                title: "n-oversized".to_string(),
                disposition: NotePackDisposition::OversizedSkipped,
                estimated_rendered_chars: None,
                estimated_rendered_tokens: None,
                action_excerpt: None,
            },
            NotePackOutcome {
                permalink: "patterns/budget".to_string(),
                title: "n-budget".to_string(),
                disposition: NotePackDisposition::BudgetPruned,
                estimated_rendered_chars: None,
                estimated_rendered_tokens: None,
                action_excerpt: None,
            },
        ],
        total_injected_chars: 0,
        total_injected_tokens: 0,
    };

    let candidates = trace_candidates_from_pack(&notes, &packed);
    assert_eq!(candidates.len(), 2);

    // Assert on the serialized trace payload, not only the Rust enum.
    let serialized = serde_json::to_value(&candidates).expect("serialize trace candidates");
    let rows = serialized.as_array().expect("candidate array");
    assert_eq!(rows[0]["outcome"].as_str(), Some("skipped"));
    assert_eq!(
        rows[0]["skipped_reason"].as_str(),
        Some("oversized_skipped")
    );
    assert_eq!(rows[1]["outcome"].as_str(), Some("skipped"));
    assert_eq!(rows[1]["skipped_reason"].as_str(), Some("budget_pruned"));

    // And the persisted vocabulary must validate against the DB contract.
    assert!(
        djinn_db::repositories::retrieval_trace::validate_candidates(&candidates).is_ok(),
        "oversized_skipped must be an accepted skipped_reason"
    );
}

/// End-to-end: a note that `rendered_line` DROPS (fixed overhead alone exceeds
/// the per-line cap, so nothing at all is rendered) must reach the trace as
/// `oversized_skipped`. Without this the drop was indistinguishable from a
/// budget loss and the deletion was invisible to the operator.
#[test]
fn dropped_note_surfaces_as_oversized_skipped_end_to_end() {
    use crate::actors::slot::helpers::{
        KnowledgePackConfig, NotePackDisposition, pack_ranked_knowledge_notes,
    };
    use djinn_slot::helpers::rendered_line_overhead_bytes;

    // A long permalink makes the fixed per-line overhead exceed any small cap.
    let dropped = oversize_test_note("n-dropped", &format!("patterns/{}", "x".repeat(200)), 0.9);
    let kept = oversize_test_note("n-kept", "patterns/short", 0.8);

    // Cap the line just under the dropped note's fixed overhead, but well
    // above the kept note's, so exactly one note is un-renderable.
    let line_byte_cap = rendered_line_overhead_bytes(&dropped) - 1;
    assert!(
        rendered_line_overhead_bytes(&kept) < line_byte_cap,
        "the short note's overhead must still fit the cap"
    );

    let notes = vec![dropped, kept];
    let packed = pack_ranked_knowledge_notes(
        &notes,
        KnowledgePackConfig {
            minimum_confidence: f64::NEG_INFINITY,
            top_k: notes.len(),
            total_byte_budget: 100_000,
            line_byte_cap,
        },
    );
    assert_eq!(
        packed.outcomes[0].disposition,
        NotePackDisposition::OversizedSkipped,
        "precondition: packing drops the note whole"
    );
    assert_eq!(
        packed.outcomes[1].disposition,
        NotePackDisposition::Injected
    );

    let candidates = trace_candidates_from_pack(&notes, &packed);
    let serialized = serde_json::to_value(&candidates).expect("serialize trace candidates");
    let rows = serialized.as_array().expect("candidate array");
    assert_eq!(
        rows[0]["skipped_reason"].as_str(),
        Some("oversized_skipped"),
        "a silently dropped note must be visible as oversized_skipped"
    );
    assert_eq!(rows[1]["outcome"].as_str(), Some("injected"));
    assert!(rows[1]["skipped_reason"].is_null());
}

#[test]
fn classify_candidates_for_error_marks_all_search_error() {
    let candidates = vec![
        tc("n1", "p/n1", "Note 1", 0.9, 1),
        tc("n2", "p/n2", "Note 2", 0.5, 2),
    ];
    let result = classify_knowledge_candidates_for_error(&candidates);
    assert_eq!(result.len(), 2);
    for c in &result {
        assert_eq!(c.outcome, CandidateOutcome::Skipped);
        assert_eq!(c.skipped_reason, Some(SkippedReason::SearchError));
    }
}

// ── Ranked-search failure records `search_error` and injects nothing ────────
//
// Proposal 5205 removed the separate trace-candidate query: one ranked search
// now produces both the prompt input and the trace universe. There is
// therefore no longer a "trace query failed but production succeeded" state to
// test. What replaces it is AC10's contract: a ranked-search failure injects no
// knowledge, records `search_error`, and does not prevent prompt construction.
//
// A NULL `confidence` column is still the cheapest way to break the query —
// the ranked path hydrates candidates into a non-nullable `f64`, so
// `sqlx::FromRow` fails.

#[tokio::test]
async fn ranked_search_failure_records_search_error_and_injects_nothing() {
    let mut env = knowledge_context_test_env_guard();
    env.clear();
    let db = djinn_db::Database::ephemeral().await.expect("ephemeral db");
    let events = EventBus::noop();
    let task =
        create_project_epic_task(&db, &events, "TC search fail epic", "TC search fail task").await;
    let project_id = task.project_id.clone();

    let note_a = seed_scoped_note(&db, &task, "TC fail A", "[]", 0.85).await;
    seed_scoped_note(&db, &task, "TC fail B", "[]", 0.75).await;
    djinn_db::test_support::nullify_note_confidence_for_test(&db, &note_a.id).await;

    let app_state = agent_context_from_db(db.clone(), CancellationToken::new());

    // Nothing is injected — and, critically, no stale unranked fallback is
    // substituted.
    let result = load_knowledge_context(&task, None, &app_state, None).await;
    assert!(
        result.is_none(),
        "a ranked-search failure must inject no knowledge, not fall back"
    );

    let trace = latest_trace(&db, &project_id).await.expect("error trace");
    assert_exceptional_terminal(&trace, RetrievalTraceOutcome::Error);
    assert!(trace.candidates_typed().is_empty());

    // The trace must say *why*, and must be distinguishable from a legitimate
    // zero-result retrieval.
    let trigger = trace.trigger.expect("trigger present");
    assert!(
        trigger["search_error"].is_string(),
        "the trace must record search_error, got {trigger}"
    );
    assert_eq!(trigger["strategy"], "ranked_injection_v1");
}

/// The same failure must not stop the *rest* of the prompt from being built.
///
/// `assemble_prompt_context` composes knowledge with several other sections; a
/// retrieval failure may only blank the knowledge block.
#[tokio::test]
async fn ranked_search_failure_still_allows_prompt_construction() {
    let mut env = knowledge_context_test_env_guard();
    env.clear();
    let db = djinn_db::Database::ephemeral().await.expect("ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Prod fail epic", "Prod fail task").await;

    let note = seed_scoped_note(&db, &task, "Prod fail note", "[]", 0.9).await;
    djinn_db::test_support::nullify_note_confidence_for_test(&db, &note.id).await;

    let app_state = agent_context_from_db(db.clone(), CancellationToken::new());

    // The call returns rather than panicking or propagating, and yields None.
    let knowledge = load_knowledge_context(&task, None, &app_state, None).await;
    assert!(knowledge.is_none());

    // A second call is equally non-fatal — the failure is not sticky.
    let again = load_knowledge_context(&task, None, &app_state, None).await;
    assert!(again.is_none());
}

// ── One ranked list, packed exactly once ───────────────────────────────────
//
// AC6: exactly one at-most-50-item ordered list reaches
// `pack_ranked_knowledge_notes`, and the confidence floor, top-k, and byte
// budget are applied once over it. This test reproduces the production
// composition independently — the same ranked search, then the same packer —
// and requires byte-identical output. It replaces the pre-5205 version, which
// compared against `query_by_scope_overlap` + `pack_knowledge_notes`; that
// comparison is no longer meaningful because injection no longer uses that
// query.

#[tokio::test]
async fn load_knowledge_context_rendered_matches_the_ranked_pack() {
    let mut env = knowledge_context_test_env_guard();
    env.clear();

    let db = djinn_db::Database::ephemeral().await.expect("ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Match epic", "Match task").await;
    let project_id = task.project_id.clone();

    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());

    let high_note = note_repo
        .create(
            &project_id,
            "High Confidence Note",
            &related_content(&task, "high body"),
            "pattern",
            "[]",
        )
        .await
        .unwrap();
    set_note_confidence(&db, &high_note.id, 0.95).await;

    let low_note = note_repo
        .create(
            &project_id,
            "Low Confidence Note",
            &related_content(&task, "low body"),
            "pitfall",
            "[]",
        )
        .await
        .unwrap();
    set_note_confidence(&db, &low_note.id, 0.5).await;

    let app_state = agent_context_from_db(db.clone(), CancellationToken::new());
    let rendered = load_knowledge_context(&task, None, &app_state, None)
        .await
        .expect("knowledge context should be Some");

    // Reproduce the production composition: the ranked search, then the packer.
    let top_k = app_state.knowledge_injection.knowledge_injection_limit as usize;
    let search = note_repo
        .search_knowledge_injection_candidates(
            djinn_db::repositories::note::KnowledgeInjectionSearchParams {
                project_id: &project_id,
                query: &crate::actors::slot::lifecycle::prompt_context::knowledge_injection_query(
                    &task, None,
                ),
                task_id: Some(&task.id),
                note_types: KNOWLEDGE_NOTE_TYPES,
                task_paths: &[],
                top_k,
                semantic_scores: None,
            },
        )
        .await
        .expect("ranked search");
    assert!(
        search.candidates.len() <= search.candidate_window,
        "at most one window of candidates may reach packing"
    );

    let notes: Vec<djinn_memory::Note> = search
        .candidates
        .iter()
        .map(|candidate| candidate.note.clone())
        .collect();
    let expected = crate::actors::slot::helpers::pack_ranked_knowledge_notes(
        &notes,
        crate::actors::slot::helpers::KnowledgePackConfig {
            minimum_confidence: KNOWLEDGE_MIN_CONFIDENCE,
            top_k,
            total_byte_budget: app_state
                .knowledge_injection
                .knowledge_injection_budget_bytes as usize,
            line_byte_cap: app_state
                .knowledge_injection
                .knowledge_injection_line_cap_bytes as usize,
        },
    )
    .rendered;

    assert_eq!(
        rendered, expected,
        "the rendered prompt must be exactly one ranked list packed exactly once"
    );

    // R1: a line is labelled by the note's permalink, not its title.
    assert!(rendered.contains(&high_note.permalink));
    assert!(rendered.contains(&low_note.permalink));
}

#[tokio::test]
async fn successful_trace_replays_oversized_rank_one_with_taxonomy_v1_histogram() {
    let mut env = knowledge_context_test_env_guard();
    env.clear();
    let db = djinn_db::Database::ephemeral().await.expect("ephemeral db");
    let events = EventBus::noop();
    let task =
        create_project_epic_task(&db, &events, "Oversized replay epic", "Oversized replay").await;
    let project_id = task.project_id.clone();
    let repo = NoteRepository::new(db.clone(), EventBus::noop());

    // Rank order is now decided by relevance fusion, not by `confidence DESC`,
    // so this test no longer pins *which* note lands where. Every assertion
    // below is instead order-independent by construction:
    //
    // * `oversized` can never render at any budget (its permalink alone
    //   overruns the 128-byte line cap), so its disposition does not depend on
    //   its rank — provided it is inside top-k, which `top_k = 5` guarantees.
    // * `low` is below the 0.3 floor, and the floor is applied before rank.
    // * each fitting note renders a full 128-byte line, so with a 256-byte
    //   budget exactly one fits (128 + 1 separator + 128 = 257 > 256)
    //   regardless of which one is first.
    let oversized = repo
        .create(
            &project_id,
            &"O".repeat(100),
            &related_content(&task, "metadata overflow"),
            "pattern",
            "[]",
        )
        .await
        .unwrap();
    let fitting: Vec<djinn_memory::Note> = {
        let mut notes = Vec::new();
        for index in 0..3 {
            notes.push(
                repo.create(
                    &project_id,
                    &format!("Fitting candidate {index}"),
                    &related_content(&task, &"a".repeat(400)),
                    "pattern",
                    "[]",
                )
                .await
                .unwrap(),
            );
        }
        notes
    };
    let low = repo
        .create(
            &project_id,
            "Below confidence threshold",
            &related_content(&task, "low"),
            "pattern",
            "[]",
        )
        .await
        .unwrap();
    set_note_confidence(&db, &oversized.id, 0.975).await;
    for note in &fitting {
        set_note_confidence(&db, &note.id, 0.97).await;
    }
    set_note_confidence(&db, &low.id, 0.1).await;

    let mut app_state = agent_context_from_db(db.clone(), CancellationToken::new());
    app_state.knowledge_injection = djinn_core::models::KnowledgeInjectionConfig {
        knowledge_injection_budget_bytes: 256,
        knowledge_injection_line_cap_bytes: 128,
        // Every confidence-eligible candidate is inside top-k, so no candidate
        // is `not_top_k` and the remaining dispositions are rank-independent.
        knowledge_injection_limit: 5,
        ..Default::default()
    };
    let rendered = load_knowledge_context(&task, None, &app_state, None)
        .await
        .expect("fitting candidate survives");
    assert_eq!(rendered.len(), 128, "exactly one full line fits the budget");

    let trace = latest_trace(&db, &project_id)
        .await
        .expect("terminal trace");
    assert_eq!(trace.knowledge_trace_taxonomy_version, Some(1));
    assert_eq!(trace.terminal_state.as_deref(), Some("success"));
    assert_eq!(trace.candidate_count, Some(5));
    // The u46i contract this test exists for: an oversized note is reported
    // distinctly from a budget-pruned one, and every candidate is accounted
    // for exactly once.
    assert_eq!(
        (
            trace.confidence_filtered_count,
            trace.not_top_k_count,
            trace.oversized_skipped_count,
            trace.injected_count,
            trace.budget_pruned_count
        ),
        (Some(1), Some(0), Some(1), Some(1), Some(2))
    );

    let candidates = trace.candidates_typed();
    assert_eq!(candidates.len(), 5);
    // Fused ranks are 1-based and contiguous, whatever the order turned out
    // to be.
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.rank)
            .collect::<Vec<_>>(),
        (1..=5).map(Some).collect::<Vec<_>>()
    );
    // Dispositions pinned by identity — each of these holds at any rank.
    let disposition_of = |note_id: &str| {
        candidates
            .iter()
            .find(|candidate| candidate.note_id == note_id)
            .map(|candidate| (candidate.outcome, candidate.skipped_reason))
            .expect("candidate present")
    };
    assert_eq!(
        disposition_of(&oversized.id),
        (
            CandidateOutcome::Skipped,
            Some(SkippedReason::OversizedSkipped)
        ),
        "a note that can never render is oversized_skipped, never budget_pruned"
    );
    assert_eq!(
        disposition_of(&low.id),
        (
            CandidateOutcome::Skipped,
            Some(SkippedReason::MinConfidence)
        ),
        "the confidence floor is applied before rank"
    );
    // The single injected note is one of the fitting ones, and it is the note
    // the prompt actually renders.
    let injected_id = candidates
        .iter()
        .find(|candidate| candidate.outcome == CandidateOutcome::Injected)
        .map(|candidate| candidate.note_id.clone())
        .expect("exactly one injected candidate");
    let injected_note = fitting
        .iter()
        .find(|note| note.id == injected_id)
        .expect("the injected note must be one of the fitting candidates");
    assert!(rendered.contains(&injected_note.permalink));
}

#[tokio::test]
async fn successful_trace_charges_newline_and_persists_exact_budget_equality() {
    let mut env = knowledge_context_test_env_guard();
    env.clear();
    let db = djinn_db::Database::ephemeral().await.expect("ephemeral db");
    let events = EventBus::noop();
    let task =
        create_project_epic_task(&db, &events, "Exact budget epic", "Exact budget task").await;
    let project_id = task.project_id.clone();
    let repo = NoteRepository::new(db.clone(), EventBus::noop());
    let first = repo
        .create(
            &project_id,
            "First exact line",
            &related_content(&task, &"x".repeat(400)),
            "pattern",
            "[]",
        )
        .await
        .unwrap();
    let second = repo
        .create(
            &project_id,
            "Second exact line",
            &related_content(&task, &"y".repeat(400)),
            "pattern",
            "[]",
        )
        .await
        .unwrap();
    set_note_confidence(&db, &first.id, 0.99).await;
    set_note_confidence(&db, &second.id, 0.98).await;
    let mut app_state = agent_context_from_db(db.clone(), CancellationToken::new());
    app_state.knowledge_injection = djinn_core::models::KnowledgeInjectionConfig {
        knowledge_injection_budget_bytes: 257,
        knowledge_injection_line_cap_bytes: 128,
        knowledge_injection_limit: 2,
        ..Default::default()
    };
    let rendered = load_knowledge_context(&task, None, &app_state, None)
        .await
        .expect("two exact-budget lines");
    assert_eq!(rendered.len(), 257, "budget includes the separator byte");
    assert_eq!(
        rendered.split('\n').map(str::len).collect::<Vec<_>>(),
        vec![128, 128]
    );
    assert_eq!(rendered.as_bytes()[128], b'\n');
    let trace = latest_trace(&db, &project_id)
        .await
        .expect("terminal trace");
    assert_eq!(trace.knowledge_trace_taxonomy_version, Some(1));
    assert_eq!(trace.terminal_state.as_deref(), Some("success"));
    assert_eq!(trace.candidate_count, Some(2));
    assert_eq!(trace.injected_count, Some(2));
    assert_eq!(
        (
            trace.confidence_filtered_count,
            trace.not_top_k_count,
            trace.oversized_skipped_count,
            trace.budget_pruned_count
        ),
        (Some(0), Some(0), Some(0), Some(0))
    );
}

#[tokio::test]
async fn cancelled_knowledge_load_persists_cancelled_terminal_without_prompt_text() {
    let mut env = knowledge_context_test_env_guard();
    env.clear();
    let db = djinn_db::Database::ephemeral().await.expect("ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Cancelled epic", "Cancelled task").await;
    let project_id = task.project_id.clone();
    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    note_repo
        .create(
            &project_id,
            "Would have been injected",
            "content",
            "pattern",
            "[]",
        )
        .await
        .expect("seed note");

    let app_state = agent_context_from_db(db.clone(), CancellationToken::new());
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let rollout = knowledge_context_rollout_from_env();

    let rendered = load_knowledge_context_with_planner(
        &task,
        None,
        &app_state,
        None,
        &rollout,
        &cancellation,
        None,
    )
    .await;

    assert!(
        rendered.is_none(),
        "cancelled retrieval must not inject prompt text"
    );
    let trace = latest_trace(&db, &project_id)
        .await
        .expect("cancelled terminal trace");
    assert_eq!(trace.knowledge_trace_taxonomy_version, Some(1));
    assert_eq!(trace.outcome, RetrievalTraceOutcome::Error);
    assert_eq!(trace.terminal_state.as_deref(), Some("cancelled"));
    assert!(trace.terminal_at.is_some());
    assert_eq!(trace.candidate_count, None);
    assert_eq!(trace.injected_count, None);
    assert_eq!(trace.confidence_filtered_count, None);
    assert_eq!(trace.not_top_k_count, None);
    assert_eq!(trace.oversized_skipped_count, None);
    assert_eq!(trace.budget_pruned_count, None);
}
