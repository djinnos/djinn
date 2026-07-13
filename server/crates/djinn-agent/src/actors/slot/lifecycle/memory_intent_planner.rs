//! Typed, injectable boundary for the optional session-start memory planner.
//!
//! This follows F6's useful design precedent: prompt rendering and parsing are
//! pure, a fake is injected for tests, and disabled callers can short-circuit
//! before any prompt work. It deliberately does **not** import
//! `djinn_graph::query_planner`: F6's synchronous code-search expansion and
//! score-union contract is target-specific, while memory planning needs typed,
//! async queries and must preserve the existing memory-search scoring pipeline.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::context::MemoryIntentPlannerConfig;

/// Stable prompt identifier used for attribution by the future provider wiring.
pub const MEMORY_INTENT_PLANNER_PROMPT_ID: &str = "memory-intent-planner-v1";
/// The planner always emits a deliberately small, bounded fan-out.
pub const MIN_PLANNED_QUERIES: usize = 2;
pub const MAX_PLANNED_QUERIES: usize = 4;
const PROMPT_TEMPLATE: &str = include_str!("../../../native_assets/memory-intent-planner-v1.md");

/// Input assembled at session start. All fields are already parsed by the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannerInput {
    pub title: String,
    pub description: String,
    pub acceptance_criteria: Vec<String>,
    /// Intentionally untruncated: the caller owns any compaction policy.
    pub resume_compaction_summary: Option<String>,
}

type FakeSearchResults<N> = Vec<Result<Vec<N>, PlannerError>>;

/// The closed set of note categories safe for planner-directed retrieval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlannedNoteType {
    Pitfall,
    Pattern,
    Case,
    Reference,
}

/// One validated request for the existing memory search pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedQuery {
    pub note_type: PlannedNoteType,
    pub query: String,
}

/// Materialized work for an enabled planner call. Constructing this value is
/// intentionally the first point at which the stable prompt is rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedPlannerRequest {
    pub input: PlannerInput,
    pub prompt: String,
}

/// Apply the default-off gate before doing any prompt work. The caller can pass
/// the resulting request to its provider seam, or skip all planner work on
/// `None` without a model, database, or network dependency.
pub fn prepare_planner_request(
    config: &MemoryIntentPlannerConfig,
    input: PlannerInput,
) -> Option<PreparedPlannerRequest> {
    if !config.is_enabled() {
        return None;
    }
    let prompt = render_prompt(&input);
    Some(PreparedPlannerRequest { input, prompt })
}

/// Render the versioned planner prompt without I/O or hidden ambient state.
pub fn render_prompt(input: &PlannerInput) -> String {
    let criteria = if input.acceptance_criteria.is_empty() {
        "- (none)".to_string()
    } else {
        input
            .acceptance_criteria
            .iter()
            .map(|criterion| format!("- {criterion}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let resume = input
        .resume_compaction_summary
        .as_deref()
        .map(|summary| format!("## Resume compaction summary\n{summary}"))
        .unwrap_or_default();

    PROMPT_TEMPLATE
        .replace("{{title}}", &input.title)
        .replace("{{description}}", &input.description)
        .replace("{{acceptance_criteria}}", &criteria)
        .replace("{{resume_compaction_summary}}", &resume)
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum PlannerError {
    #[error("planner response is not valid JSON: {0}")]
    InvalidJson(String),
    #[error(
        "planner response must contain {MIN_PLANNED_QUERIES}–{MAX_PLANNED_QUERIES} queries, got {0}"
    )]
    WrongQueryCount(usize),
    #[error("planner query {index} is invalid: {reason}")]
    InvalidQuery { index: usize, reason: &'static str },
    #[error("planner invocation failed: {0}")]
    Invocation(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlannerPayload {
    queries: Vec<RawPlannedQuery>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPlannedQuery {
    #[serde(rename = "type")]
    note_type: PlannedNoteType,
    query: String,
}

/// Strictly parse a model payload. Query strings are never rewritten, so exact
/// error strings, symbols, and config keys remain discriminative search terms.
pub fn parse_planned_queries(raw: &str) -> Result<Vec<PlannedQuery>, PlannerError> {
    let payload: PlannerPayload =
        serde_json::from_str(raw).map_err(|error| PlannerError::InvalidJson(error.to_string()))?;
    if !(MIN_PLANNED_QUERIES..=MAX_PLANNED_QUERIES).contains(&payload.queries.len()) {
        return Err(PlannerError::WrongQueryCount(payload.queries.len()));
    }

    payload
        .queries
        .into_iter()
        .enumerate()
        .map(|(index, raw)| {
            validate_query(&raw.query)
                .map_err(|reason| PlannerError::InvalidQuery { index, reason })?;
            Ok(PlannedQuery {
                note_type: raw.note_type,
                query: raw.query,
            })
        })
        .collect()
}

/// Phase-1-compatible query-style predicate kept local to avoid introducing a
/// dependency from the host lifecycle crate onto MCP schema construction.
pub fn validate_query(query: &str) -> Result<(), &'static str> {
    if query.is_empty() || query != query.trim() {
        return Err("query must be non-empty and have no surrounding whitespace");
    }
    let lower = query.to_ascii_lowercase();
    if query.ends_with('?')
        || [
            "can ", "could ", "would ", "should ", "what ", "where ", "when ", "why ", "how ",
            "is ", "are ", "do ", "does ",
        ]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
    {
        return Err("query must be declarative, not interrogative");
    }
    if contains_phrase(
        &lower,
        &[
            "find",
            "search for",
            "information about",
            "look up",
            "retrieve",
        ],
    ) {
        return Err("query contains retrieval-meta wording");
    }
    if contains_word(
        &lower,
        &["this", "that", "these", "those", "it", "they", "them"],
    ) {
        return Err("query is not self-contained");
    }
    if lower.contains("; ")
        || lower.contains(" and ")
        || lower.contains(" or ")
        || lower.contains(" versus ")
    {
        return Err("query must express one information need");
    }
    Ok(())
}

fn contains_phrase(query: &str, phrases: &[&str]) -> bool {
    phrases.iter().any(|phrase| {
        let mut offset = 0;
        while let Some(found) = query[offset..].find(phrase) {
            let start = offset + found;
            let end = start + phrase.len();
            if is_boundary(query, start, end) {
                return true;
            }
            offset = end;
        }
        false
    })
}

fn contains_word(query: &str, words: &[&str]) -> bool {
    contains_phrase(query, words)
}

fn is_boundary(text: &str, start: usize, end: usize) -> bool {
    let before = text[..start].chars().next_back();
    let after = text[end..].chars().next();
    before.is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_')
        && after.is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_')
}

/// Async provider seam. Production provider/cost wiring belongs to a later task.
#[async_trait]
pub trait MemoryIntentPlanner: Send + Sync {
    async fn plan(&self, input: PlannerInput) -> Result<String, PlannerError>;
}

/// Async existing-search seam. It deliberately accepts a typed query rather
/// than implementing ranking, so the repository's scoring remains unchanged.
#[async_trait]
pub trait PlannedNoteSearch: Send + Sync {
    type Note: Send + Sync;
    async fn search(&self, query: PlannedQuery) -> Result<Vec<Self::Note>, PlannerError>;
}

/// Deterministic test planner that records calls and can delay its response.
#[derive(Clone)]
pub struct FakeMemoryIntentPlanner {
    result: Result<String, PlannerError>,
    delay: Duration,
    calls: Arc<Mutex<Vec<PlannerInput>>>,
}

impl FakeMemoryIntentPlanner {
    pub fn new(result: Result<String, PlannerError>) -> Self {
        Self {
            result,
            delay: Duration::ZERO,
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    pub async fn calls(&self) -> Vec<PlannerInput> {
        self.calls.lock().await.clone()
    }
}

#[async_trait]
impl MemoryIntentPlanner for FakeMemoryIntentPlanner {
    async fn plan(&self, input: PlannerInput) -> Result<String, PlannerError> {
        self.calls.lock().await.push(input);
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        self.result.clone()
    }
}

/// Deterministic search fake with one result bucket per call, useful for testing
/// ordering and delayed parallel consumers without a database.
#[derive(Clone)]
pub struct FakePlannedNoteSearch<N: Clone + Send + Sync> {
    results: Arc<Mutex<FakeSearchResults<N>>>,
    delay: Duration,
    calls: Arc<Mutex<Vec<PlannedQuery>>>,
}

impl<N: Clone + Send + Sync> FakePlannedNoteSearch<N> {
    pub fn new(results: Vec<Result<Vec<N>, PlannerError>>) -> Self {
        Self {
            results: Arc::new(Mutex::new(results)),
            delay: Duration::ZERO,
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
    pub async fn calls(&self) -> Vec<PlannedQuery> {
        self.calls.lock().await.clone()
    }
}

#[async_trait]
impl<N: Clone + Send + Sync + 'static> PlannedNoteSearch for FakePlannedNoteSearch<N> {
    type Note = N;

    async fn search(&self, query: PlannedQuery) -> Result<Vec<N>, PlannerError> {
        self.calls.lock().await.push(query);
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        let mut results = self.results.lock().await;
        if results.is_empty() {
            Ok(Vec::new())
        } else {
            results.remove(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> PlannerInput {
        PlannerInput {
            title: "Task title".into(),
            description: "Task description".into(),
            acceptance_criteria: vec!["Criterion one".into()],
            resume_compaction_summary: Some("Untruncated resume text".into()),
        }
    }
    fn valid() -> &'static str {
        r#"{"queries":[{"type":"pitfall","query":"Database migration timeout E_CONNRESET"},{"type":"pattern","query":"Memory planner configuration injection"}]}"#
    }

    #[test]
    fn prompt_is_deterministic_and_includes_all_input() {
        let rendered = render_prompt(&input());
        assert_eq!(rendered, render_prompt(&input()));
        for expected in [
            "Task title",
            "Task description",
            "- Criterion one",
            "Untruncated resume text",
        ] {
            assert!(rendered.contains(expected));
        }
    }

    #[test]
    fn disabled_config_short_circuits_before_prompt_preparation() {
        assert!(prepare_planner_request(&MemoryIntentPlannerConfig::default(), input()).is_none());
        let config = MemoryIntentPlannerConfig {
            enabled: true,
            ..Default::default()
        };
        assert!(prepare_planner_request(&config, input()).is_some());
    }

    #[test]
    fn parser_accepts_typed_phase_one_style_queries() {
        let queries = parse_planned_queries(valid()).unwrap();
        assert_eq!(queries[0].note_type, PlannedNoteType::Pitfall);
        assert_eq!(queries[0].query, "Database migration timeout E_CONNRESET");
    }

    #[test]
    fn parser_rejects_all_invalid_payload_classes() {
        for raw in [
            "not json",
            r#"{"queries":[{"type":"unknown","query":"Valid statement"},{"type":"case","query":"Another valid statement"}]}"#,
            r#"{"queries":[{"type":"case","query":"Only one"}]}"#,
            r#"{"queries":[{"type":"case","query":"Can you find migration failures?"},{"type":"case","query":"Another valid statement"}]}"#,
            r#"{"queries":[{"type":"case","query":"This migration failure"},{"type":"case","query":"Another valid statement"}]}"#,
            r#"{"queries":[{"type":"case","query":"Migration retries and timeout policy"},{"type":"case","query":"Another valid statement"}]}"#,
        ] {
            assert!(parse_planned_queries(raw).is_err(), "{raw}");
        }
    }

    #[tokio::test]
    async fn fakes_record_inputs_and_support_delays() {
        let planner =
            FakeMemoryIntentPlanner::new(Ok(valid().into())).with_delay(Duration::from_millis(1));
        assert_eq!(planner.plan(input()).await.unwrap(), valid());
        assert_eq!(planner.calls().await.len(), 1);
        let search =
            FakePlannedNoteSearch::new(vec![Ok(vec!["note"])]).with_delay(Duration::from_millis(1));
        assert_eq!(
            search
                .search(parse_planned_queries(valid()).unwrap().remove(0))
                .await
                .unwrap(),
            vec!["note"]
        );
        assert_eq!(search.calls().await.len(), 1);
    }
}
