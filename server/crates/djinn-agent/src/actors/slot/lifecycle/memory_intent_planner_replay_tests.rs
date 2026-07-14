//! Checked-in, network-free replay corpus for the session-start planner seam.

use std::collections::HashMap;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::Mutex;

use super::memory_intent_planner::{
    FakeMemoryIntentPlanner, PlannedContextNote, PlannedNoteSearch, PlannedQuery,
    PlannerCallOutcome, PlannerError, PlannerInput, PlannerLedger, SessionStartPlannerResult,
    run_session_start_memory_planner,
};
use crate::context::MemoryIntentPlannerConfig;

const FIXTURES: &str =
    include_str!("../../../../tests/fixtures/memory_intent_planner/replay_cases.json");
const SCOPE_ONLY: &str = "scope-only";
const AVAILABLE_ATTEMPTED_USAGE: u32 = 17;

#[derive(Debug, Deserialize)]
struct ReplayCase {
    name: String,
    enabled: bool,
    provider: String,
    #[serde(default)]
    payload: Option<String>,
    #[serde(default)]
    finalization_fails: bool,
    #[serde(default)]
    full_scope_budget: bool,
    #[serde(default)]
    bucket_mode: String,
    #[serde(default)]
    resume_compaction_summary: Option<String>,
    expected_outcome: String,
    expected_context: String,
    expected_available_usage: u32,
    expected_accounting_finalized: bool,
    #[serde(default)]
    expected_ledger_outcome: Option<String>,
}

#[derive(Default)]
struct ReplayLedger {
    records: Mutex<Vec<(PlannerCallOutcome, u32)>>,
    finalization_fails: bool,
}

#[async_trait]
impl PlannerLedger for ReplayLedger {
    async fn record(
        &self,
        outcome: PlannerCallOutcome,
        available_usage: u32,
    ) -> Result<(), PlannerError> {
        self.records.lock().await.push((outcome, available_usage));
        if self.finalization_fails {
            return Err(PlannerError::Invocation(
                "ledger finalization failed".into(),
            ));
        }
        Ok(())
    }
}
impl ReplayLedger {
    async fn records(&self) -> Vec<(PlannerCallOutcome, u32)> {
        self.records.lock().await.clone()
    }
}

/// Query-keyed fake exercises the final `PlannedNoteSearch` seam without a database.
#[derive(Default)]
struct ReplaySearch {
    buckets: HashMap<String, Vec<PlannedContextNote>>,
    calls: Mutex<Vec<PlannedQuery>>,
}
#[async_trait]
impl PlannedNoteSearch for ReplaySearch {
    type Note = PlannedContextNote;
    async fn search(&self, query: PlannedQuery) -> Result<Vec<Self::Note>, PlannerError> {
        let notes = self.buckets.get(&query.query).cloned().unwrap_or_default();
        self.calls.lock().await.push(query);
        Ok(notes)
    }
}
impl ReplaySearch {
    async fn calls(&self) -> Vec<PlannedQuery> {
        self.calls.lock().await.clone()
    }
}

fn input(case: &ReplayCase) -> PlannerInput {
    PlannerInput {
        title: "Replay task".into(),
        description: "Network-free deterministic replay validation".into(),
        acceptance_criteria: vec!["Planner output remains scope-first".into()],
        resume_compaction_summary: case.resume_compaction_summary.clone(),
    }
}
fn provider_result(case: &ReplayCase) -> Result<String, PlannerError> {
    match case.provider.as_str() {
        "success" => Ok(case.payload.clone().unwrap_or_default()),
        "timeout" => Err(PlannerError::Invocation("timeout".into())),
        "provider_error" => Err(PlannerError::Invocation("provider error".into())),
        other => panic!("unknown fixture provider {other}"),
    }
}
fn note(id: impl Into<String>, rendered: impl Into<String>) -> PlannedContextNote {
    let id = id.into();
    PlannedContextNote {
        permalink: format!("memory/{id}"),
        id,
        rendered: rendered.into(),
    }
}
fn normal_buckets() -> HashMap<String, Vec<PlannedContextNote>> {
    HashMap::from([
        (
            "Database migration timeout E_CONNRESET".into(),
            vec![note(
                "planner-first",
                "- **[Pitfall] first**: ranked one (permalink: pitfall/first)",
            )],
        ),
        (
            "Memory planner configuration injection".into(),
            vec![note(
                "planner-second",
                "- **[Pattern] second**: ranked two (permalink: pattern/second)",
            )],
        ),
    ])
}
fn buckets(case: &ReplayCase) -> HashMap<String, Vec<PlannedContextNote>> {
    match case.bucket_mode.as_str() {
        "duplicates" => HashMap::from([
            (
                "Database migration timeout E_CONNRESET".into(),
                vec![note("scope-note", "planner duplicate by id")],
            ),
            (
                "Memory planner configuration injection".into(),
                vec![PlannedContextNote {
                    id: "other-id".into(),
                    permalink: "memory/scope-note".into(),
                    rendered: "planner duplicate by permalink".into(),
                }],
            ),
        ]),
        "caps" => (1..=4)
            .map(|query| {
                (
                    format!("Replay cap query {query}"),
                    (1..=3)
                        .map(|rank| note(format!("q{query}-r{rank}"), format!("q{query}-r{rank}")))
                        .collect(),
                )
            })
            .collect(),
        _ => normal_buckets(),
    }
}
fn scope_context(case: &ReplayCase) -> String {
    if case.full_scope_budget {
        "x".repeat(2_000)
    } else {
        SCOPE_ONLY.into()
    }
}
fn outcome_name(outcome: Option<PlannerCallOutcome>) -> String {
    match outcome {
        None => "disabled".into(),
        Some(PlannerCallOutcome::Success) => "success".into(),
        Some(PlannerCallOutcome::Timeout) => "timeout".into(),
        Some(PlannerCallOutcome::ProviderError) => "provider_error".into(),
        Some(PlannerCallOutcome::InvalidPayload) => "invalid_payload".into(),
    }
}
fn expected_outcome(name: &str) -> PlannerCallOutcome {
    match name {
        "success" => PlannerCallOutcome::Success,
        "timeout" => PlannerCallOutcome::Timeout,
        "provider_error" => PlannerCallOutcome::ProviderError,
        "invalid_payload" => PlannerCallOutcome::InvalidPayload,
        other => panic!("unknown expected ledger outcome {other}"),
    }
}
struct ReplayResult {
    result: SessionStartPlannerResult,
    planner_calls: usize,
    planner_inputs: Vec<PlannerInput>,
    search_calls: Vec<PlannedQuery>,
    ledger_records: Vec<(PlannerCallOutcome, u32)>,
}

async fn replay(case: &ReplayCase) -> ReplayResult {
    let config = MemoryIntentPlannerConfig {
        enabled: case.enabled,
        ..Default::default()
    };
    let planner = FakeMemoryIntentPlanner::new(provider_result(case));
    let search = ReplaySearch {
        buckets: buckets(case),
        ..Default::default()
    };
    let ledger = ReplayLedger {
        finalization_fails: case.finalization_fails,
        ..Default::default()
    };
    let (scope_ids, scope_permalinks) = if case.bucket_mode == "duplicates" {
        (vec!["scope-note".into()], vec!["memory/scope-note".into()])
    } else {
        (Vec::new(), Vec::new())
    };
    let result = run_session_start_memory_planner(
        &config,
        input(case),
        scope_context(case),
        &scope_ids,
        &scope_permalinks,
        &planner,
        &search,
        &ledger,
        AVAILABLE_ATTEMPTED_USAGE,
    )
    .await;
    let planner_inputs = planner.calls().await;
    ReplayResult {
        result,
        planner_calls: planner_inputs.len(),
        planner_inputs,
        search_calls: search.calls().await,
        ledger_records: ledger.records().await,
    }
}

#[tokio::test]
async fn checked_in_memory_intent_planner_replays_use_final_injected_seams() {
    let cases: Vec<ReplayCase> = serde_json::from_str(FIXTURES).expect("checked-in replay corpus");
    assert_eq!(cases.len(), 13, "keep the rollout matrix exhaustive");
    for case in &cases {
        let first = replay(case).await;
        let second = replay(case).await;
        assert_eq!(
            first.result.context, case.expected_context,
            "{} context",
            case.name
        );
        assert_eq!(
            outcome_name(first.result.outcome),
            case.expected_outcome,
            "{} outcome",
            case.name
        );
        assert_eq!(
            first.result.available_usage, case.expected_available_usage,
            "{} usage",
            case.name
        );
        assert_eq!(
            first.result.accounting_finalized, case.expected_accounting_finalized,
            "{} accounting finalization",
            case.name
        );
        assert_eq!(
            first.result, second.result,
            "{} replay bytes/outcome drifted",
            case.name
        );
        assert_eq!(
            first.ledger_records, second.ledger_records,
            "{} durable accounting drifted",
            case.name
        );
        if let Some(expected) = &case.expected_ledger_outcome {
            assert_eq!(
                first.ledger_records,
                vec![(expected_outcome(expected), case.expected_available_usage)],
                "{} durable ledger outcome/usage",
                case.name
            );
        } else {
            assert!(
                first.ledger_records.is_empty(),
                "{} must not record",
                case.name
            );
        }
        if !case.enabled {
            assert_eq!(
                first.planner_calls, 0,
                "disabled mode must not attempt planning"
            );
            assert!(
                first.search_calls.is_empty(),
                "disabled mode must not search"
            );
        } else {
            assert_eq!(first.planner_calls, 1, "{} planner attempt", case.name);
        }
        if matches!(
            case.expected_outcome.as_str(),
            "timeout" | "provider_error" | "invalid_payload"
        ) || case.finalization_fails
        {
            assert_eq!(
                first.result.context, SCOPE_ONLY,
                "{} must fail open",
                case.name
            );
            assert!(
                first.search_calls.is_empty(),
                "{} must not search",
                case.name
            );
        }
        if case.full_scope_budget {
            assert_eq!(
                first.result.context.len(),
                2_000,
                "scope consumes full budget"
            );
            assert_eq!(
                first.search_calls.len(),
                2,
                "merge must calculate zero remainder after search"
            );
        }
        if case.bucket_mode == "caps" {
            assert_eq!(
                first.result.context,
                "scope-only\nq1-r1\nq1-r2\nq2-r1\nq2-r2\nq3-r1\nq3-r2"
            );
            assert_eq!(first.search_calls.len(), 4, "all ordered query buckets run");
        }
        if let Some(summary) = &case.resume_compaction_summary {
            assert_eq!(
                first.planner_inputs[0].resume_compaction_summary.as_deref(),
                Some(summary.as_str())
            );
        }
    }
}

#[test]
fn replay_corpus_includes_each_required_rollout_path() {
    let cases: Vec<ReplayCase> = serde_json::from_str(FIXTURES).expect("checked-in replay corpus");
    let names: std::collections::HashSet<&str> =
        cases.iter().map(|case| case.name.as_str()).collect();
    for required in [
        "enabled_success",
        "disabled_no_work",
        "timeout",
        "provider_error",
        "malformed_json",
        "unknown_type",
        "wrong_query_count",
        "query_style_invalid",
        "accounting_finalization_failure",
        "duplicate_collapse",
        "full_scope_budget",
        "cap_limits",
        "resume_compaction_input",
    ] {
        assert!(
            names.contains(required),
            "missing replay fixture {required}"
        );
    }
}
