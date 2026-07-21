//! Checked-in, network-free replays through the production prompt-assembly boundary.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use async_trait::async_trait;
use djinn_core::events::EventBus;
use djinn_db::{Database, NoteRepository};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use super::prompt_context::test_support::create_project_epic_task;
use super::prompt_context::{
    MemoryIntentPlannerHost, MemoryIntentPlannerInvocation, PlannedNoteSearch, PromptContext,
    PromptContextInputs, assemble_prompt_context, knowledge_context_test_env_guard,
};
use crate::context::{AgentContext, MemoryIntentPlannerConfig};
use crate::roles::LeadRole;
use crate::test_helpers::{agent_context_from_db, test_tempdir};
use djinn_supervisor::services::wire::{
    AttributedPlannerRequest, PlannerAttemptResult, PlannerOutcome,
};

const FIXTURES: &str =
    include_str!("../../../../tests/fixtures/memory_intent_planner/replay_cases.json");
const AVAILABLE_ATTEMPTED_USAGE: i64 = 17;

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
    knowledge_injection_limit: Option<u32>,
    #[serde(default)]
    bucket_mode: String,
    #[serde(default)]
    resume_compaction_summary: Option<String>,
    expected_outcome: String,
    expected_context: String,
    expected_ledger_outcome: String,
    expected_available_usage: u32,
    expected_accounting_finalized: bool,
}

struct ReplayHost {
    result: PlannerAttemptResult,
    requests: Mutex<Vec<AttributedPlannerRequest>>,
}

#[async_trait]
impl MemoryIntentPlannerHost for ReplayHost {
    async fn plan_memory_intents(
        &self,
        request: AttributedPlannerRequest,
    ) -> Result<PlannerAttemptResult, String> {
        self.requests.lock().expect("host requests").push(request);
        Ok(self.result.clone())
    }
}

#[derive(Default)]
struct ReplaySearch {
    buckets: HashMap<String, Vec<djinn_memory::MemorySearchEntityRow>>,
    requests: Mutex<Vec<(String, String)>>,
}

#[async_trait]
impl PlannedNoteSearch for ReplaySearch {
    async fn search_planned_notes(
        &self,
        _project_id: &str,
        _task_id: &str,
        query: &str,
        note_type: &str,
    ) -> Result<Vec<djinn_memory::MemorySearchEntityRow>, String> {
        self.requests
            .lock()
            .expect("search requests")
            .push((query.to_owned(), note_type.to_owned()));
        Ok(self.buckets.get(query).cloned().unwrap_or_default())
    }
}

fn row(
    id: &str,
    title: &str,
    permalink: &str,
    snippet: &str,
) -> djinn_memory::MemorySearchEntityRow {
    djinn_memory::MemorySearchEntityRow {
        entity: "note".into(),
        id: id.into(),
        title: title.into(),
        folder: "replay".into(),
        note_type: "pattern".into(),
        permalink: permalink.into(),
        snippet: snippet.into(),
        score: 1.0,
    }
}

fn outcome(name: &str) -> PlannerOutcome {
    match name {
        "success" => PlannerOutcome::Success,
        "timeout" => PlannerOutcome::Timeout,
        "provider_error" => PlannerOutcome::ProviderError,
        "invalid_payload" => PlannerOutcome::InvalidPayload,
        other => panic!("unknown replay outcome {other}"),
    }
}

fn host(case: &ReplayCase) -> ReplayHost {
    let provider_outcome = if case.expected_outcome == "disabled" {
        PlannerOutcome::Success
    } else {
        outcome(&case.expected_outcome)
    };
    ReplayHost {
        result: PlannerAttemptResult {
            outcome: if case.finalization_fails {
                PlannerOutcome::ProviderError
            } else {
                provider_outcome
            },
            // Completed provider calls retain their raw payload so the production
            // parser/style validator is replayed even when durable accounting has
            // already classified the completion as invalid_payload.
            content: (case.provider == "success" && !case.finalization_fails)
                .then(|| case.payload.clone())
                .flatten(),
            tokens_in: 10,
            tokens_out: 7,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost_usd: Some(0.001),
            diagnostic: case
                .finalization_fails
                .then(|| "ledger finalization failed: fixture".into()),
        },
        requests: Mutex::new(Vec::new()),
    }
}

fn normal_buckets() -> HashMap<String, Vec<djinn_memory::MemorySearchEntityRow>> {
    HashMap::from([
        (
            "Database migration timeout E_CONNRESET".into(),
            vec![row("planner-first", "First", "pitfall/first", "ranked one")],
        ),
        (
            "Memory planner configuration injection".into(),
            vec![row(
                "planner-second",
                "Second",
                "pattern/second",
                "ranked two",
            )],
        ),
    ])
}

fn buckets(
    case: &ReplayCase,
    scope_id: &str,
    scope_permalink: &str,
) -> HashMap<String, Vec<djinn_memory::MemorySearchEntityRow>> {
    match case.bucket_mode.as_str() {
        "duplicates" => {
            let shared = row("shared-planned", "Shared planned", "pattern/shared", "once");
            HashMap::from([
                (
                    "Database migration timeout E_CONNRESET".into(),
                    vec![
                        row(scope_id, "Scope ID duplicate", "other/link", "skip"),
                        shared.clone(),
                    ],
                ),
                (
                    "Memory planner configuration injection".into(),
                    vec![
                        row(
                            "other-id",
                            "Scope permalink duplicate",
                            scope_permalink,
                            "skip",
                        ),
                        shared,
                        row("after-shared", "After shared", "pattern/after", "unique"),
                    ],
                ),
            ])
        }
        "caps" => (1..=4)
            .map(|query| {
                (
                    format!("Replay cap query {query}"),
                    (1..=3)
                        .map(|rank| {
                            let key = format!("q{query}-r{rank}");
                            row(&key, &key, &format!("pattern/{key}"), &key)
                        })
                        .collect(),
                )
            })
            .collect(),
        _ if case.full_scope_budget => normal_buckets()
            .into_keys()
            .map(|query| {
                (
                    query,
                    vec![row(
                        "short-planned",
                        "Short planned",
                        "pattern/short",
                        "this short row fits with a larger remainder",
                    )],
                )
            })
            .collect(),
        _ => normal_buckets(),
    }
}

async fn assemble(
    task: &djinn_core::models::Task,
    state: &AgentContext,
    planner: Option<MemoryIntentPlannerInvocation<'_>>,
    worktree_path: &std::path::Path,
) -> PromptContext {
    let role = LeadRole;
    assemble_prompt_context(PromptContextInputs {
        task,
        runtime_role: &role,
        role_for_epic_check: &role,
        project_path: "/workspace/replay",
        worktree_path,
        conflict_ctx: None,
        merge_validation_ctx: None,
        prompt_setup_commands: None,
        system_prompt_extensions: "",
        resolved_skills: &[],
        app_state: state,
        read_sources: &[],
        worker_resume_note: None,
        arbiter_directive: None,
        mcp_server_instructions: &BTreeMap::new(),
        extension_diagnostics: &[],
        memory_intent_planner: planner,
    })
    .await
}

#[tokio::test]
async fn checked_in_replays_enter_the_production_assemble_prompt_context_boundary() {
    let _knowledge_context_env = knowledge_context_test_env_guard();
    let cases: Vec<ReplayCase> = serde_json::from_str(FIXTURES).expect("checked-in replay corpus");
    assert_eq!(cases.len(), 13, "keep the rollout matrix exhaustive");

    for case in &cases {
        let db = Database::ephemeral().await.expect("ephemeral replay db");
        let events = EventBus::noop();
        let mut task = create_project_epic_task(&db, &events, "Replay epic", "Replay task").await;
        task.description = "Network-free deterministic replay validation".into();
        task.created_by_user_id = "replay-creator".into();

        let note_repo = NoteRepository::new(db.clone(), events);
        let scope = note_repo
            .create(
                &task.project_id,
                "Scope Only",
                "scope baseline",
                "pattern",
                "[]",
            )
            .await
            .expect("seed scope note");
        if case.full_scope_budget {
            note_repo
                .update_summaries(&scope.id, None, Some(&"x".repeat(1_900)))
                .await
                .expect("seed near-full scope overview");
        }
        note_repo
            .set_confidence(&scope.id, 0.95)
            .await
            .expect("scope confidence");

        let mut state = agent_context_from_db(db, CancellationToken::new());
        if let Some(limit) = case.knowledge_injection_limit {
            state.knowledge_injection.knowledge_injection_limit = limit;
        }
        let worktree = test_tempdir("memory-planner-replay-");
        let baseline = assemble(&task, &state, None, worktree.path()).await;
        let host = host(case);
        let search = ReplaySearch {
            buckets: buckets(case, &scope.id, &scope.permalink),
            ..Default::default()
        };
        let config = MemoryIntentPlannerConfig {
            enabled: case.enabled,
            ..Default::default()
        };
        let run = || {
            assemble(
                &task,
                &state,
                Some(MemoryIntentPlannerInvocation {
                    config: &config,
                    host: &host,
                    session_id: "replay-session",
                    task_run_id: "replay-run",
                    creator_id: Some(task.created_by_user_id.as_str()),
                    acceptance_criteria: vec!["Planner output remains scope-first".into()],
                    resume_compaction_summary: case.resume_compaction_summary.as_deref(),
                    planned_note_search: Some(&search),
                }),
                worktree.path(),
            )
        };
        let first = run().await;
        let second = run().await;

        assert_eq!(
            first.knowledge_context, second.knowledge_context,
            "{} context drift",
            case.name
        );
        assert_eq!(
            first.system_prompt, second.system_prompt,
            "{} prompt drift",
            case.name
        );
        assert_eq!(
            first.knowledge_context.as_deref(),
            Some(case.expected_context.as_str()),
            "{} exact production-rendered context",
            case.name
        );

        let requests = host.requests.lock().expect("host requests");
        if !case.enabled {
            assert!(requests.is_empty(), "disabled mode records no attempt");
            assert_eq!(case.expected_ledger_outcome, "disabled");
            assert_eq!(first.knowledge_context, baseline.knowledge_context);
            assert_eq!(first.system_prompt, baseline.system_prompt);
            assert!(search.requests.lock().expect("search requests").is_empty());
            continue;
        }
        assert_eq!(requests.len(), 2, "{} host attempts", case.name);
        let attempted = &host.result;
        assert_eq!(
            attempted.outcome,
            outcome(&case.expected_ledger_outcome),
            "{} durable outcome",
            case.name
        );
        assert_eq!(
            attempted.tokens_in + attempted.tokens_out,
            case.expected_available_usage as i64
        );
        assert_eq!(
            attempted.tokens_in + attempted.tokens_out,
            AVAILABLE_ATTEMPTED_USAGE
        );
        assert_eq!(
            !attempted
                .diagnostic
                .as_deref()
                .is_some_and(|d| d.starts_with("ledger finalization failed")),
            case.expected_accounting_finalized,
            "{} finalization",
            case.name
        );

        let failure = case.expected_outcome != "success" || case.finalization_fails;
        if failure {
            assert_eq!(
                first.knowledge_context, baseline.knowledge_context,
                "{} scope-only fallback",
                case.name
            );
        }
        if failure {
            assert!(
                search.requests.lock().expect("search requests").is_empty(),
                "{} must not search",
                case.name
            );
        }
        if case.bucket_mode == "duplicates" {
            let rendered = first
                .knowledge_context
                .as_deref()
                .expect("knowledge context");
            assert_eq!(
                rendered.matches("Shared planned").count(),
                1,
                "cross-query duplicate"
            );
            assert!(rendered.contains("After shared"));
            assert!(!rendered.contains("Scope ID duplicate"));
            assert!(!rendered.contains("Scope permalink duplicate"));
        }
        if case.bucket_mode == "caps" {
            let rendered = first
                .knowledge_context
                .as_deref()
                .expect("knowledge context");
            assert_eq!(rendered.matches("**[Note]").count(), 6);
            for kept in ["q1-r1", "q1-r2", "q2-r1", "q2-r2", "q3-r1", "q3-r2"] {
                assert!(rendered.contains(kept), "missing {kept}");
            }
            for omitted in ["q1-r3", "q2-r3", "q3-r3", "q4-r1", "q4-r2", "q4-r3"] {
                assert!(!rendered.contains(omitted), "unexpected {omitted}");
            }
        }
        if case.full_scope_budget {
            let rendered_baseline = baseline
                .knowledge_context
                .as_deref()
                .expect("scope baseline");
            assert!(
                rendered_baseline.contains("scope baseline"),
                "L0 abstract must be rendered instead of the L1 overview"
            );
            let rendered = first
                .knowledge_context
                .as_deref()
                .expect("planned knowledge context");
            assert!(rendered.contains("Short planned"));
            assert!(rendered.len() <= 2_000);
            assert_eq!(search.requests.lock().expect("search requests").len(), 4);
        }
        if let Some(summary) = &case.resume_compaction_summary {
            assert!(
                requests
                    .iter()
                    .all(|request| request.conversation.contains(summary))
            );
        }
        match attempted.outcome {
            PlannerOutcome::Timeout => assert_eq!(case.provider, "timeout"),
            PlannerOutcome::ProviderError if !case.finalization_fails => {
                assert_eq!(case.provider, "provider_error")
            }
            _ => {}
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
