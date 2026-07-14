//! Checked-in, network-free replay corpus for the default-off memory planner.
//!
//! This deliberately exercises the public fake planner/search seams rather than
//! a provider or repository. It is a replay gate for rollout evidence, not a
//! second implementation of retrieval scoring.

use futures::future::join_all;
use serde::Deserialize;

use super::memory_intent_planner::{
    FakeMemoryIntentPlanner, FakePlannedNoteSearch, MemoryIntentPlanner, PlannedNoteSearch,
    PlannerError, PlannerInput, parse_planned_queries, prepare_planner_request,
};
use crate::context::MemoryIntentPlannerConfig;

const FIXTURES: &str =
    include_str!("../../../../tests/fixtures/memory_intent_planner/replay_cases.json");
const SCOPE_ONLY: &str = "scope-only";
const AVAILABLE_ATTEMPTED_USAGE: u32 = 17;
const PLANNER_BUDGET: usize = 2_000;
const PER_QUERY_CAP: usize = 2;
const GLOBAL_CAP: usize = 6;

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
    resume_compaction_summary: Option<String>,
    expected_outcome: String,
    expected_context: String,
    expected_available_usage: u32,
}

#[derive(Debug, Clone)]
struct ReplayNote {
    id: &'static str,
    permalink: &'static str,
    label: &'static str,
    title: &'static str,
    snippet: &'static str,
}

impl ReplayNote {
    fn render(&self) -> String {
        format!(
            "- **[{}] {}**: {} (permalink: {})",
            self.label, self.title, self.snippet, self.permalink
        )
    }
}

/// Final fake seam for the durable-attribution boundary. It intentionally
/// models available attempted usage independently from final injection.
#[derive(Default)]
struct FakePlannerLedger {
    outcomes: Vec<String>,
    available_usage: u32,
    finalization_fails: bool,
}

impl FakePlannerLedger {
    fn record_attempt(&mut self, outcome: &str) {
        self.outcomes.push(outcome.to_string());
        self.available_usage = AVAILABLE_ATTEMPTED_USAGE;
    }

    fn finalize(&mut self) -> bool {
        !self.finalization_fails
    }
}

struct ReplayResult {
    context: String,
    outcome: String,
    available_usage: u32,
    planner_calls: usize,
    search_calls: usize,
    ledger_outcomes: Vec<String>,
    rendered_request: Option<String>,
}

fn input(case: &ReplayCase) -> PlannerInput {
    PlannerInput {
        title: "Replay task".into(),
        description: "Network-free deterministic replay validation".into(),
        acceptance_criteria: vec!["Planner output remains scope-first".into()],
        resume_compaction_summary: case.resume_compaction_summary.clone(),
    }
}

fn buckets(case: &ReplayCase) -> Vec<Result<Vec<ReplayNote>, PlannerError>> {
    let first = ReplayNote {
        id: "planner-first",
        permalink: "pitfall/first",
        label: "Pitfall",
        title: "first",
        snippet: "ranked one",
    };
    let second = ReplayNote {
        id: "planner-second",
        permalink: "pattern/second",
        label: "Pattern",
        title: "second",
        snippet: "ranked two",
    };
    if case.name == "duplicate_collapse" {
        return vec![
            Ok(vec![first.clone(), first]),
            Ok(vec![ReplayNote {
                id: "other-id",
                permalink: "pitfall/first",
                ..second
            }]),
        ];
    }
    vec![Ok(vec![first]), Ok(vec![second])]
}

fn render_planner_notes(
    buckets: Vec<Vec<ReplayNote>>,
    scope_used: usize,
    scope_ids: &[&str],
    scope_permalinks: &[&str],
) -> String {
    let mut ids: std::collections::HashSet<_> = scope_ids.iter().copied().collect();
    let mut permalinks: std::collections::HashSet<_> = scope_permalinks.iter().copied().collect();
    let mut used = scope_used;
    let mut lines = Vec::new();
    for bucket in buckets {
        for note in bucket.into_iter().take(PER_QUERY_CAP) {
            if !ids.insert(note.id) || !permalinks.insert(note.permalink) {
                continue;
            }
            if lines.len() == GLOBAL_CAP {
                return lines.join("\n");
            }
            let line = note.render();
            if used + line.len() > PLANNER_BUDGET {
                return lines.join("\n");
            }
            used += line.len() + 1;
            lines.push(line);
        }
    }
    lines.join("\n")
}

async fn replay(case: &ReplayCase) -> ReplayResult {
    let config = MemoryIntentPlannerConfig {
        enabled: case.enabled,
        ..Default::default()
    };
    let request = prepare_planner_request(&config, input(case));
    let rendered_request = request.as_ref().map(|request| request.prompt.clone());
    let planner_result = match case.provider.as_str() {
        // Disabled fixtures intentionally omit a provider payload: the fake is
        // constructed but never called after the default-off gate returns.
        "success" => Ok(case.payload.clone().unwrap_or_default()),
        "timeout" => Err(PlannerError::Invocation("timeout".into())),
        "provider_error" => Err(PlannerError::Invocation("provider error".into())),
        other => panic!("unknown fixture provider {other}"),
    };
    let planner = FakeMemoryIntentPlanner::new(planner_result);
    let search = FakePlannedNoteSearch::new(buckets(case));
    let mut ledger = FakePlannerLedger {
        finalization_fails: case.finalization_fails,
        ..Default::default()
    };

    let Some(request) = request else {
        return ReplayResult {
            context: SCOPE_ONLY.into(),
            outcome: "disabled".into(),
            available_usage: 0,
            planner_calls: 0,
            search_calls: 0,
            ledger_outcomes: vec![],
            rendered_request: None,
        };
    };

    let raw = planner.plan(request.input).await;
    let outcome = match (&case.provider[..], raw) {
        ("timeout", _) => "timeout",
        ("provider_error", _) => "provider_error",
        ("success", Ok(raw)) if parse_planned_queries(&raw).is_err() => "invalid_payload",
        ("success", Ok(_)) => "success",
        _ => "provider_error",
    };
    ledger.record_attempt(outcome);
    if !ledger.finalize() {
        ledger
            .outcomes
            .push("accounting_finalization_failed".into());
        return ReplayResult {
            context: SCOPE_ONLY.into(),
            outcome: "accounting_finalization_failed".into(),
            available_usage: ledger.available_usage,
            planner_calls: planner.calls().await.len(),
            search_calls: search.calls().await.len(),
            ledger_outcomes: ledger.outcomes,
            rendered_request,
        };
    }
    if outcome != "success" || case.full_scope_budget {
        return ReplayResult {
            context: SCOPE_ONLY.into(),
            outcome: outcome.into(),
            available_usage: ledger.available_usage,
            planner_calls: planner.calls().await.len(),
            search_calls: search.calls().await.len(),
            ledger_outcomes: ledger.outcomes,
            rendered_request,
        };
    }

    let queries = parse_planned_queries(&case.payload.clone().expect("success payload"))
        .expect("success fixture validates");
    let found = join_all(queries.into_iter().map(|query| search.search(query))).await;
    let notes = found
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .expect("fixture search succeeds");
    let (scope_ids, scope_permalinks): (&[&str], &[&str]) = if case.name == "duplicate_collapse" {
        // The first bucket duplicates scope by ID; the second then duplicates
        // it by permalink. Neither may displace the scope-only baseline.
        (&["planner-first"], &["pitfall/first"])
    } else {
        (&[], &[])
    };
    let planner_context =
        render_planner_notes(notes, SCOPE_ONLY.len() + 1, scope_ids, scope_permalinks);
    let context = if planner_context.is_empty() {
        SCOPE_ONLY.into()
    } else {
        format!("{SCOPE_ONLY}\n{planner_context}")
    };
    ReplayResult {
        context,
        outcome: outcome.into(),
        available_usage: ledger.available_usage,
        planner_calls: planner.calls().await.len(),
        search_calls: search.calls().await.len(),
        ledger_outcomes: ledger.outcomes,
        rendered_request,
    }
}

#[tokio::test]
async fn checked_in_memory_intent_planner_replays_are_byte_stable() {
    let cases: Vec<ReplayCase> = serde_json::from_str(FIXTURES).expect("checked-in replay corpus");
    assert_eq!(cases.len(), 12, "keep the rollout matrix exhaustive");

    for case in &cases {
        let first = replay(case).await;
        let second = replay(case).await;
        assert_eq!(
            first.context, case.expected_context,
            "{} context",
            case.name
        );
        assert_eq!(
            first.outcome, case.expected_outcome,
            "{} outcome",
            case.name
        );
        assert_eq!(
            first.available_usage, case.expected_available_usage,
            "{} usage",
            case.name
        );
        assert_eq!(
            first.context, second.context,
            "{} rendered bytes drifted",
            case.name
        );
        assert_eq!(
            first.outcome, second.outcome,
            "{} outcome drifted",
            case.name
        );
        assert_eq!(
            first.ledger_outcomes, second.ledger_outcomes,
            "{} accounting drifted",
            case.name
        );

        if !case.enabled {
            assert_eq!(
                first.planner_calls, 0,
                "disabled mode must not attempt planning"
            );
            assert_eq!(
                first.search_calls, 0,
                "disabled mode must not search planner buckets"
            );
            assert!(
                first.ledger_outcomes.is_empty(),
                "disabled mode must not account"
            );
            assert!(
                first.rendered_request.is_none(),
                "disabled mode must not render a prompt"
            );
        } else {
            assert_eq!(first.planner_calls, 1, "{} planner attempt", case.name);
            assert!(
                first.ledger_outcomes.first().is_some(),
                "{} attributed attempt",
                case.name
            );
        }
        if matches!(
            case.expected_outcome.as_str(),
            "timeout" | "provider_error" | "invalid_payload" | "accounting_finalization_failed"
        ) {
            assert_eq!(
                first.context, SCOPE_ONLY,
                "{} must fail open to scope baseline",
                case.name
            );
        }
        if case.full_scope_budget || case.finalization_fails {
            assert_eq!(
                first.search_calls, 0,
                "{} must suppress planner injection",
                case.name
            );
        }
        if let Some(summary) = &case.resume_compaction_summary {
            assert!(
                first
                    .rendered_request
                    .expect("enabled request")
                    .contains(summary),
                "resume input must remain untruncated"
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
        "resume_compaction_input",
    ] {
        assert!(
            names.contains(required),
            "missing replay fixture {required}"
        );
    }
}
