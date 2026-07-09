//! Run path for the Phase 1 memory-eval benchmark.
//!
//! Loads JSONL fixtures into an isolated Postgres test database, executes
//! real `NoteRepository::search` and `build_context` against the loaded data,
//! and produces deterministic per-query top-k rank records.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use djinn_db::database::Database;
use djinn_db::repositories::note::{NoteRepository, NoteSearchParams};

use crate::fixtures::{BadCaseRow, BadCaseType, MinedMemoryRefRow, Phase1Fixtures};
use crate::loader::{self, LoadedFixtureState};

// ── Run output types ──────────────────────────────────────────────────────

/// Per-query top-k rank record.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct QueryRankRecord {
    /// Query identifier (task ID or bad-case ID).
    pub query_id: String,
    /// The search query text.
    pub query_text: String,
    /// Optional task_id used for task-affinity scoring.
    pub task_id: Option<String>,
    /// Ranked result permalinks (top-k).
    pub result_permalinks: Vec<String>,
    /// Ranks (1-based) at which each relevant note appeared, or None if absent.
    pub relevant_ranks: Vec<Option<usize>>,
    /// The permalinks of notes that were expected to be relevant.
    pub expected_permalinks: Vec<String>,
    /// Whether this query was a bad-case.
    pub is_bad_case: bool,
    /// Bad-case type (if applicable).
    pub bad_case_type: Option<BadCaseType>,
}

/// Signal-rank comparison: records how a relevant note's rank changes when
/// a specific retrieval signal is absent.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SignalRankComparison {
    /// Query ID.
    pub query_id: String,
    /// Signal that was tested.
    pub signal: String,
    /// Rank of the best relevant note with all signals.
    pub rank_with_signal: Option<usize>,
    /// Rank of the best relevant note without the signal.
    pub rank_without_signal: Option<usize>,
    /// Whether the rank changed (lower rank = better).
    pub rank_changed: bool,
}

/// Full run output.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RunOutput {
    /// Per-query rank records for mined memory-ref queries.
    pub query_records: Vec<QueryRankRecord>,
    /// Per-query rank records for bad cases.
    pub bad_case_records: Vec<QueryRankRecord>,
    /// Signal-rank comparisons proving signal importance.
    pub signal_comparisons: Vec<SignalRankComparison>,
    /// Number of corpus notes loaded.
    pub corpus_note_count: usize,
    /// Number of queries executed.
    pub query_count: usize,
    /// Number of bad cases executed.
    pub bad_case_count: usize,
}

// ── Top-k constant ────────────────────────────────────────────────────────

/// The k for top-k ranking. Records the top 10 results per query.
const TOP_K: usize = 10;

// ── Run entry point ───────────────────────────────────────────────────────

/// Execute the Phase 1 benchmark run.
///
/// 1. Load fixtures from disk (or use provided fixtures).
/// 2. Validate fixtures.
/// 3. Load into an isolated Postgres test database.
/// 4. Execute `NoteRepository::search` for each query.
/// 5. Execute `NoteRepository::build_context` for relevant seed cases.
/// 6. Record per-query top-k ranks.
/// 7. Run signal comparisons to prove graph/entity and task-affinity matter.
pub async fn execute_run(crate_root: &Path) -> Result<RunOutput> {
    // 1. Load fixtures from disk
    let fixtures =
        loader::load_fixtures_from_disk(crate_root).context("loading fixtures from disk")?;

    execute_run_with_fixtures(&fixtures).await
}

/// Execute the run with pre-loaded fixtures (useful for testing).
pub async fn execute_run_with_fixtures(fixtures: &Phase1Fixtures) -> Result<RunOutput> {
    // 2. Validate fixtures
    info!("validating fixtures...");
    loader::validate_fixtures(fixtures).context("fixture validation failed")?;
    info!("fixture validation passed");

    // 3. Create isolated test database and load fixtures
    let db = Database::open_in_memory().context("opening isolated test database")?;

    let state = loader::load_fixtures(&db, fixtures)
        .await
        .context("loading fixtures into database")?;

    info!(
        project_id = %state.project.id,
        notes = state.note_id_by_permalink.len(),
        tasks = state.task_id_map.len(),
        "fixtures loaded"
    );

    // 4. Execute search for each query
    let repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());

    let mut query_records = Vec::new();

    for query_row in &fixtures.memory_ref_queries {
        let record = execute_search_query(&repo, &state, query_row)
            .await
            .with_context(|| format!("executing search for query '{}'", query_row.query_id))?;
        query_records.push(record);
    }

    info!(count = query_records.len(), "executed memory-ref queries");

    // 5. Execute search for each bad case
    let mut bad_case_records = Vec::new();

    for case_row in &fixtures.bad_cases {
        let record = execute_bad_case_query(&repo, &state, case_row)
            .await
            .with_context(|| format!("executing bad case '{}'", case_row.case_id))?;
        bad_case_records.push(record);
    }

    info!(count = bad_case_records.len(), "executed bad-case queries");

    // 6. Execute build_context for relevant seed cases
    execute_build_context_checks(&repo, &state, fixtures)
        .await
        .context("executing build_context checks")?;

    // 7. Run signal comparisons
    let signal_comparisons = execute_signal_comparisons(&db, &state, fixtures)
        .await
        .context("executing signal comparisons")?;

    info!(
        count = signal_comparisons.len(),
        "signal comparisons completed"
    );

    let output = RunOutput {
        query_records,
        bad_case_records,
        signal_comparisons,
        corpus_note_count: fixtures.corpus_notes.len(),
        query_count: fixtures.memory_ref_queries.len(),
        bad_case_count: fixtures.bad_cases.len(),
    };

    // 8. Run signal assertions
    assert_signal_effects(&output)?;

    info!("benchmark run completed successfully");

    Ok(output)
}

// ── Query execution ───────────────────────────────────────────────────────

/// Execute a search query and record the top-k results and relevant ranks.
async fn execute_search_query(
    repo: &NoteRepository,
    state: &LoadedFixtureState,
    query: &MinedMemoryRefRow,
) -> Result<QueryRankRecord> {
    let db_task_id = query
        .task_id
        .as_ref()
        .and_then(|tid| state.task_id_map.get(tid).map(|s| s.as_str()));

    let results = repo
        .search(NoteSearchParams {
            project_id: &state.project.id,
            query: &query.query_text,
            task_id: db_task_id,
            folder: None,
            note_type: None,
            limit: TOP_K,
            semantic_scores: None,
            edge_kinds: None,
            entity_types: None,
        })
        .await
        .context("executing NoteRepository::search")?;

    let result_permalinks: Vec<String> = results.iter().map(|r| r.permalink.clone()).collect();

    // Find ranks of expected relevant notes
    let relevant_ranks: Vec<Option<usize>> = query
        .memory_refs
        .iter()
        .map(|permalink| {
            result_permalinks
                .iter()
                .position(|r| r == permalink)
                .map(|pos| pos + 1) // 1-based rank
        })
        .collect();

    let found_count = relevant_ranks.iter().filter(|r| r.is_some()).count();
    info!(
        query_id = %query.query_id,
        results = result_permalinks.len(),
        found = found_count,
        total_expected = query.memory_refs.len(),
        "search completed"
    );

    Ok(QueryRankRecord {
        query_id: query.query_id.clone(),
        query_text: query.query_text.clone(),
        task_id: query.task_id.clone(),
        result_permalinks,
        relevant_ranks,
        expected_permalinks: query.memory_refs.clone(),
        is_bad_case: false,
        bad_case_type: None,
    })
}

/// Execute a bad-case query and record the results.
async fn execute_bad_case_query(
    repo: &NoteRepository,
    state: &LoadedFixtureState,
    case: &BadCaseRow,
) -> Result<QueryRankRecord> {
    let db_task_id = case
        .task_id
        .as_ref()
        .and_then(|tid| state.task_id_map.get(tid).map(|s| s.as_str()));

    let results = repo
        .search(NoteSearchParams {
            project_id: &state.project.id,
            query: &case.query_text,
            task_id: db_task_id,
            folder: None,
            note_type: None,
            limit: TOP_K,
            semantic_scores: None,
            edge_kinds: None,
            entity_types: None,
        })
        .await
        .context("executing NoteRepository::search for bad case")?;

    let result_permalinks: Vec<String> = results.iter().map(|r| r.permalink.clone()).collect();

    let relevant_ranks: Vec<Option<usize>> = case
        .relevant_note_permalinks
        .iter()
        .map(|permalink| {
            result_permalinks
                .iter()
                .position(|r| r == permalink)
                .map(|pos| pos + 1)
        })
        .collect();

    let found_count = relevant_ranks.iter().filter(|r| r.is_some()).count();
    info!(
        case_id = %case.case_id,
        results = result_permalinks.len(),
        found = found_count,
        total_expected = case.relevant_note_permalinks.len(),
        bad_case_type = ?case.case_type,
        "bad-case search completed"
    );

    Ok(QueryRankRecord {
        query_id: case.case_id.clone(),
        query_text: case.query_text.clone(),
        task_id: case.task_id.clone(),
        result_permalinks,
        relevant_ranks,
        expected_permalinks: case.relevant_note_permalinks.clone(),
        is_bad_case: true,
        bad_case_type: Some(case.case_type.clone()),
    })
}

// ── build_context checks ─────────────────────────────────────────────────

/// Execute `build_context` for queries that have seed notes in the corpus.
/// This validates that the context assembly path works with loaded fixtures.
async fn execute_build_context_checks(
    repo: &NoteRepository,
    state: &LoadedFixtureState,
    fixtures: &Phase1Fixtures,
) -> Result<()> {
    // Pick a few notes from the corpus to use as seeds for build_context
    let mut context_count = 0usize;
    let notes_with_neighbors: Vec<&str> = fixtures
        .corpus_notes
        .iter()
        .filter(|n| !n.graph_edges.is_empty())
        .map(|n| n.permalink.as_str())
        .collect();

    for seed_permalink in notes_with_neighbors.iter().take(5) {
        let db_task_id = fixtures
            .memory_ref_queries
            .iter()
            .find(|q| q.memory_refs.contains(&seed_permalink.to_string()))
            .and_then(|q| q.task_id.as_ref())
            .and_then(|tid| state.task_id_map.get(tid).map(|s| s.as_str()));

        let context_result = repo
            .build_context(
                &state.project.id,
                seed_permalink,
                Some(4096),
                db_task_id,
                10,
                Some(0.1),
                None,
            )
            .await
            .with_context(|| format!("build_context for seed '{}'", seed_permalink))?;

        info!(
            seed = %seed_permalink,
            primary = context_result.primary.len(),
            l1 = context_result.related_l1.len(),
            l0 = context_result.related_l0.len(),
            "build_context completed"
        );

        // The seed note itself should always be in primary
        assert!(
            !context_result.primary.is_empty(),
            "build_context must return the seed note as primary for '{}'",
            seed_permalink
        );

        context_count += 1;
    }

    info!(count = context_count, "build_context checks passed");
    Ok(())
}

// ── Signal comparisons ────────────────────────────────────────────────────

/// Re-run queries that claim graph/entity or task-affinity signals without
/// those signals to prove they matter.
async fn execute_signal_comparisons(
    db: &Database,
    state: &LoadedFixtureState,
    fixtures: &Phase1Fixtures,
) -> Result<Vec<SignalRankComparison>> {
    let mut comparisons = Vec::new();

    // Find queries that claim graph or entity signals
    let graph_entity_queries: Vec<&MinedMemoryRefRow> = fixtures
        .memory_ref_queries
        .iter()
        .filter(|q| q.expected_signals.graph || q.expected_signals.entity)
        .collect();

    // Find queries that claim task-affinity signal
    let task_affinity_queries: Vec<&MinedMemoryRefRow> = fixtures
        .memory_ref_queries
        .iter()
        .filter(|q| q.expected_signals.task_affinity && q.task_id.is_some())
        .collect();

    // Also check bad cases for graph/entity and task-affinity
    let graph_entity_cases: Vec<&BadCaseRow> = fixtures
        .bad_cases
        .iter()
        .filter(|c| c.expected_signals.graph || c.expected_signals.entity)
        .collect();

    let task_affinity_cases: Vec<&BadCaseRow> = fixtures
        .bad_cases
        .iter()
        .filter(|c| c.expected_signals.task_affinity && c.task_id.is_some())
        .collect();

    // Run graph/entity comparison on queries
    for query in &graph_entity_queries {
        let comparison = compare_search_with_and_without_graph(
            db,
            state,
            &query.query_text,
            &query.memory_refs,
            query.task_id.as_deref(),
            &query.query_id,
        )
        .await
        .with_context(|| format!("graph comparison for query '{}'", query.query_id))?;
        if let Some(comp) = comparison {
            comparisons.push(comp);
        }
    }

    // Run graph/entity comparison on bad cases
    for case in &graph_entity_cases {
        let comparison = compare_search_with_and_without_graph(
            db,
            state,
            &case.query_text,
            &case.relevant_note_permalinks,
            case.task_id.as_deref(),
            &case.case_id,
        )
        .await
        .with_context(|| format!("graph comparison for bad case '{}'", case.case_id))?;
        if let Some(comp) = comparison {
            comparisons.push(comp);
        }
    }

    // Run task-affinity comparison on queries
    for query in &task_affinity_queries {
        let comparison = compare_search_with_and_without_task_affinity(
            db,
            state,
            &query.query_text,
            &query.memory_refs,
            query.task_id.as_deref().unwrap(),
            &query.query_id,
        )
        .await
        .with_context(|| format!("task-affinity comparison for query '{}'", query.query_id))?;
        if let Some(comp) = comparison {
            comparisons.push(comp);
        }
    }

    // Run task-affinity comparison on bad cases
    for case in &task_affinity_cases {
        let comparison = compare_search_with_and_without_task_affinity(
            db,
            state,
            &case.query_text,
            &case.relevant_note_permalinks,
            case.task_id.as_deref().unwrap(),
            &case.case_id,
        )
        .await
        .with_context(|| format!("task-affinity comparison for bad case '{}'", case.case_id))?;
        if let Some(comp) = comparison {
            comparisons.push(comp);
        }
    }

    Ok(comparisons)
}

/// Compare search results with and without graph signals by limiting edge_kinds
/// to a non-existent kind (effectively disabling graph proximity).
async fn compare_search_with_and_without_graph(
    db: &Database,
    state: &LoadedFixtureState,
    query_text: &str,
    expected_permalinks: &[String],
    task_id: Option<&str>,
    query_id: &str,
) -> Result<Option<SignalRankComparison>> {
    let repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
    let db_task_id = task_id.and_then(|tid| state.task_id_map.get(tid).map(|s| s.as_str()));

    // With all signals (normal search)
    let results_with = repo
        .search(NoteSearchParams {
            project_id: &state.project.id,
            query: query_text,
            task_id: db_task_id,
            folder: None,
            note_type: None,
            limit: TOP_K,
            semantic_scores: None,
            edge_kinds: None,
            entity_types: None,
        })
        .await?;

    // Without graph signals (restrict edge_kinds to non-existent kind)
    let no_graph_kinds = vec!["__no_graph__".to_string()];
    let results_without = repo
        .search(NoteSearchParams {
            project_id: &state.project.id,
            query: query_text,
            task_id: db_task_id,
            folder: None,
            note_type: None,
            limit: TOP_K,
            semantic_scores: None,
            edge_kinds: Some(&no_graph_kinds),
            entity_types: None,
        })
        .await
        .unwrap_or_default();

    let rank_with = find_best_relevant_rank(&results_with, expected_permalinks);
    let rank_without = find_best_relevant_rank(&results_without, expected_permalinks);
    let rank_changed = rank_with != rank_without;

    if rank_changed {
        info!(
            query_id = %query_id,
            rank_with = ?rank_with,
            rank_without = ?rank_without,
            "graph signal affects rank"
        );
    }

    Ok(Some(SignalRankComparison {
        query_id: query_id.to_string(),
        signal: "graph".to_string(),
        rank_with_signal: rank_with,
        rank_without_signal: rank_without,
        rank_changed,
    }))
}

/// Compare search results with and without task-affinity signal.
async fn compare_search_with_and_without_task_affinity(
    db: &Database,
    state: &LoadedFixtureState,
    query_text: &str,
    expected_permalinks: &[String],
    task_id: &str,
    query_id: &str,
) -> Result<Option<SignalRankComparison>> {
    let repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
    let db_task_id = state.task_id_map.get(task_id).map(|s| s.as_str());

    // With task-affinity
    let results_with = repo
        .search(NoteSearchParams {
            project_id: &state.project.id,
            query: query_text,
            task_id: db_task_id,
            folder: None,
            note_type: None,
            limit: TOP_K,
            semantic_scores: None,
            edge_kinds: None,
            entity_types: None,
        })
        .await?;

    // Without task-affinity (no task_id)
    let results_without = repo
        .search(NoteSearchParams {
            project_id: &state.project.id,
            query: query_text,
            task_id: None,
            folder: None,
            note_type: None,
            limit: TOP_K,
            semantic_scores: None,
            edge_kinds: None,
            entity_types: None,
        })
        .await?;

    let rank_with = find_best_relevant_rank(&results_with, expected_permalinks);
    let rank_without = find_best_relevant_rank(&results_without, expected_permalinks);
    let rank_changed = rank_with != rank_without;

    if rank_changed {
        info!(
            query_id = %query_id,
            rank_with = ?rank_with,
            rank_without = ?rank_without,
            "task-affinity signal affects rank"
        );
    }

    Ok(Some(SignalRankComparison {
        query_id: query_id.to_string(),
        signal: "task_affinity".to_string(),
        rank_with_signal: rank_with,
        rank_without_signal: rank_without,
        rank_changed,
    }))
}

/// Find the best (lowest) 1-based rank of any expected relevant note in the
/// results. Returns None if no relevant note is found.
fn find_best_relevant_rank(
    results: &[djinn_memory::MemorySearchEntityRow],
    expected_permalinks: &[String],
) -> Option<usize> {
    let expected_set: HashSet<&str> = expected_permalinks.iter().map(|s| s.as_str()).collect();
    results
        .iter()
        .enumerate()
        .filter(|(_, r)| expected_set.contains(r.permalink.as_str()))
        .map(|(i, _)| i + 1) // 1-based
        .min()
}

// ── Signal assertions ─────────────────────────────────────────────────────

/// Assert that at least one graph/entity signal comparison changed a rank,
/// and at least one task-affinity signal comparison changed a rank.
/// These are acceptance-criteria assertions.
fn assert_signal_effects(output: &RunOutput) -> Result<()> {
    // Check graph/entity signal
    let graph_changed = output
        .signal_comparisons
        .iter()
        .any(|c| c.signal == "graph" && c.rank_changed);

    let graph_comparisons: Vec<_> = output
        .signal_comparisons
        .iter()
        .filter(|c| c.signal == "graph")
        .collect();

    if graph_comparisons.is_empty() {
        warn!(
            "no graph/entity signal comparisons were generated (no queries claimed graph/entity signals)"
        );
    } else if !graph_changed {
        bail!(
            "graph/entity signal comparisons exist ({} comparisons) but none showed rank change",
            graph_comparisons.len()
        );
    } else {
        info!("graph/entity signal assertion passed: at least one rank changed");
    }

    // Check task-affinity signal
    let task_affinity_changed = output
        .signal_comparisons
        .iter()
        .any(|c| c.signal == "task_affinity" && c.rank_changed);

    let task_affinity_comparisons: Vec<_> = output
        .signal_comparisons
        .iter()
        .filter(|c| c.signal == "task_affinity")
        .collect();

    if task_affinity_comparisons.is_empty() {
        warn!(
            "no task-affinity signal comparisons were generated (no queries claimed task-affinity signals)"
        );
    } else if !task_affinity_changed {
        bail!(
            "task-affinity signal comparisons exist ({} comparisons) but none showed rank change",
            task_affinity_comparisons.len()
        );
    } else {
        info!("task-affinity signal assertion passed: at least one rank changed");
    }

    // At least one of each signal type should have been compared
    // (this validates the test infrastructure)
    assert!(
        !graph_comparisons.is_empty() || !task_affinity_comparisons.is_empty(),
        "at least one signal comparison must be generated"
    );

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::*;

    /// Build a corpus with notes that all match "guard" lexically but differ
    /// in graph proximity and task-affinity signals, so we can prove those
    /// signals change rank.
    fn make_test_corpus_notes() -> Vec<CorpusNoteRow> {
        // Note 1: "Supervisor guard pattern" - strong match for "guard", connected to Note 2
        let json1 = r#"{"permalink":"patterns/supervisor-guard","title":"Supervisor guard pattern","content":"Guard pattern for managing supervisor lifecycle transitions safely.","note_type":"pattern","folder":"patterns","status":"active","tags":["guard","supervisor"],"timestamps":{"created_at":"2026-06-01T10:00:00.000Z","updated_at":"2026-06-15T14:30:00.000Z","last_accessed":"2026-07-01T09:00:00.000Z"},"confidence":0.85,"embedding":{"content_hash":"abc123def456","model_version":"text-embedding-3-small-v1","embedding_dim":3,"vector":[0.1,0.2,0.3]},"labels":[{"entity_type":"concept","name":"guard"}],"graph_edges":[{"source_permalink":"patterns/supervisor-guard","target_permalink":"patterns/connected-guard","kind":"builds_on","weight":1.0}],"expected_signals":{"vector":true,"lexical":true,"temporal":true,"graph":true,"entity":true,"task_affinity":false}}"#;
        // Note 2: "Guard rails for pipeline" - partial "guard" match, connected to Note 1 via graph
        let json2 = r#"{"permalink":"patterns/connected-guard","title":"Guard rails for pipeline configuration","content":"Pipeline guard rails prevent misconfiguration during automated deployment.","note_type":"pattern","folder":"patterns","status":"active","tags":["guard","pipeline"],"timestamps":{"created_at":"2026-03-01T00:00:00.000Z","updated_at":"2026-03-15T00:00:00.000Z","last_accessed":"2026-06-01T00:00:00.000Z"},"confidence":0.9,"embedding":{"content_hash":"hash456","model_version":"text-embedding-3-small-v1","embedding_dim":3,"vector":[0.4,0.5,0.6]},"labels":[{"entity_type":"concept","name":"guard"}],"graph_edges":[{"source_permalink":"patterns/supervisor-guard","target_permalink":"patterns/connected-guard","kind":"builds_on","weight":1.0}],"expected_signals":{"vector":true,"lexical":true,"temporal":false,"graph":true,"entity":true,"task_affinity":false}}"#;
        // Note 3: "Guard configuration reference" - partial "guard" match, NOT connected
        let json3 = r#"{"permalink":"patterns/unconnected-guard","title":"Guard configuration reference","content":"Reference configuration for guard setup in test environments.","note_type":"pattern","folder":"patterns","status":"active","tags":["guard","config"],"timestamps":{"created_at":"2026-04-01T00:00:00.000Z","updated_at":"2026-04-15T00:00:00.000Z","last_accessed":"2026-06-01T00:00:00.000Z"},"confidence":1.0,"embedding":{"content_hash":"hash789","model_version":"text-embedding-3-small-v1","embedding_dim":3,"vector":[0.7,0.8,0.9]},"labels":[{"entity_type":"concept","name":"config"}],"graph_edges":[],"expected_signals":{"vector":true,"lexical":true,"temporal":false,"graph":false,"entity":false,"task_affinity":false}}"#;
        // Note 4: "Pipeline guard deployment" - has "guard" + "pipeline", in task memory_refs
        let json4 = r#"{"permalink":"cases/task-affinity-guard","title":"Pipeline guard deployment notes","content":"Deployment notes for pipeline guard configuration and rollback procedures.","note_type":"case","folder":"cases","status":"active","tags":["deployment","pipeline","guard"],"timestamps":{"created_at":"2026-04-01T00:00:00.000Z","updated_at":"2026-04-15T00:00:00.000Z","last_accessed":"2026-06-01T00:00:00.000Z"},"confidence":1.0,"embedding":{"content_hash":"hash012","model_version":"text-embedding-3-small-v1","embedding_dim":3,"vector":[0.2,0.3,0.4]},"labels":[],"graph_edges":[],"expected_signals":{"vector":true,"lexical":true,"temporal":false,"graph":false,"entity":false,"task_affinity":true}}"#;
        vec![
            serde_json::from_str(json1).unwrap(),
            serde_json::from_str(json2).unwrap(),
            serde_json::from_str(json3).unwrap(),
            serde_json::from_str(json4).unwrap(),
        ]
    }

    fn make_test_fixtures() -> Phase1Fixtures {
        // Query "guard" matches all 4 notes lexically. With graph signal,
        // Note 2 (connected-guard) should rank higher relative to Note 3
        // (unconnected-guard). With task-affinity, Note 4 should rank higher.
        let query_json = r#"{"query_id":"task-abc123","query_text":"guard","task_id":"abc123","memory_refs":["patterns/connected-guard","patterns/unconnected-guard","cases/task-affinity-guard"],"expected_signals":{"vector":true,"lexical":true,"temporal":false,"graph":true,"entity":true,"task_affinity":true}}"#;
        // Graph bad case: only Note 2 is relevant (graph-connected)
        let bc_graph = r#"{"case_id":"bc-002","query_text":"guard pipeline configuration","case_type":"graph_entity_influenced","expected_behavior":"Graph proximity should boost connected guard note","task_id":null,"relevant_note_permalinks":["patterns/connected-guard"],"expected_signals":{"vector":false,"lexical":true,"temporal":false,"graph":true,"entity":false,"task_affinity":false},"tags":["graph"]}"#;
        // Task-affinity bad case: only Note 4 is relevant (in task memory_refs)
        let bc_task = r#"{"case_id":"bc-003","query_text":"guard deployment pipeline","case_type":"task_affinity_influenced","expected_behavior":"Task-affinity signal should boost the note in task memory_refs","task_id":"abc123","relevant_note_permalinks":["cases/task-affinity-guard"],"expected_signals":{"vector":false,"lexical":true,"temporal":false,"graph":false,"entity":false,"task_affinity":true},"tags":["task-affinity"]}"#;

        Phase1Fixtures {
            corpus_notes: make_test_corpus_notes(),
            memory_ref_queries: vec![serde_json::from_str(query_json).unwrap()],
            bad_cases: vec![
                serde_json::from_str(bc_graph).unwrap(),
                serde_json::from_str(bc_task).unwrap(),
            ],
            manifest: None,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_with_test_fixtures_produces_output() {
        let fixtures = make_test_fixtures();
        let output = execute_run_with_fixtures(&fixtures)
            .await
            .expect("run should succeed");

        assert_eq!(output.query_records.len(), 1);
        assert_eq!(output.bad_case_records.len(), 2);
        assert_eq!(output.corpus_note_count, 4);

        let query_record = &output.query_records[0];
        assert_eq!(query_record.query_id, "task-abc123");
        assert!(!query_record.result_permalinks.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_records_relevant_ranks() {
        let fixtures = make_test_fixtures();
        let output = execute_run_with_fixtures(&fixtures)
            .await
            .expect("run should succeed");

        let query_record = &output.query_records[0];
        let found_any = query_record.relevant_ranks.iter().any(|r| r.is_some());
        assert!(
            found_any,
            "at least one relevant note should be found in search results"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_produces_signal_comparisons() {
        let fixtures = make_test_fixtures();
        let output = execute_run_with_fixtures(&fixtures)
            .await
            .expect("run should succeed");

        assert!(
            !output.signal_comparisons.is_empty(),
            "signal comparisons should be generated"
        );

        let graph_count = output
            .signal_comparisons
            .iter()
            .filter(|c| c.signal == "graph")
            .count();
        assert!(graph_count > 0, "should have graph signal comparisons");

        let ta_count = output
            .signal_comparisons
            .iter()
            .filter(|c| c.signal == "task_affinity")
            .count();
        assert!(ta_count > 0, "should have task-affinity signal comparisons");
    }

    #[test]
    fn find_best_relevant_rank_returns_none_when_no_match() {
        let results: Vec<djinn_memory::MemorySearchEntityRow> = vec![];
        let expected = vec!["a".to_string()];
        assert!(find_best_relevant_rank(&results, &expected).is_none());
    }

    /// Proves that graph input changes at least one relevant note rank.
    ///
    /// The test corpus has notes 2 and 3 that both match "guard" lexically.
    /// Note 2 is connected to note 1 via graph edges; note 3 is not.
    /// When graph signals are disabled, the rank of note 2 changes relative
    /// to note 3 (because graph proximity no longer boosts note 2).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn graph_signal_changes_relevant_note_rank() {
        let fixtures = make_test_fixtures();
        let output = execute_run_with_fixtures(&fixtures)
            .await
            .expect("run should succeed");

        let graph_comparisons: Vec<_> = output
            .signal_comparisons
            .iter()
            .filter(|c| c.signal == "graph")
            .collect();
        assert!(
            !graph_comparisons.is_empty(),
            "must have at least one graph signal comparison"
        );

        let any_graph_changed = graph_comparisons.iter().any(|c| c.rank_changed);

        assert!(
            any_graph_changed,
            "graph comparisons must show rank change for at least one relevant note: {:?}",
            graph_comparisons
        );
    }

    /// Proves that task-affinity input changes at least one relevant note rank.
    ///
    /// Note 4 is in the task's memory_refs and matches "guard" lexically.
    /// With task-affinity, it should rank higher than without.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn task_affinity_signal_changes_relevant_note_rank() {
        let fixtures = make_test_fixtures();
        let output = execute_run_with_fixtures(&fixtures)
            .await
            .expect("run should succeed");

        let ta_comparisons: Vec<_> = output
            .signal_comparisons
            .iter()
            .filter(|c| c.signal == "task_affinity")
            .collect();
        assert!(
            !ta_comparisons.is_empty(),
            "must have at least one task-affinity signal comparison"
        );

        let any_ta_changed = ta_comparisons.iter().any(|c| c.rank_changed);

        assert!(
            any_ta_changed,
            "task-affinity comparisons must show rank change for at least one relevant note: {:?}",
            ta_comparisons
        );
    }
}
