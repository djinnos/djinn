//! Typed, injectable boundary for the optional session-start memory planner.
//!
//! This follows F6's useful design precedent: prompt rendering and parsing are
//! pure, a fake is injected for tests, and disabled callers can short-circuit
//! before any prompt work. It deliberately does **not** import
//! `djinn_graph::query_planner`: F6's synchronous code-search expansion and
//! score-union contract is target-specific, while memory planning needs typed,
//! async queries and must preserve the existing memory-search scoring pipeline.

use serde::Deserialize;

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
///
/// Enforces parity with the Phase 1 memory_search contract delivered in
/// `djinn-mcp-extension/src/shared_schemas.rs::tool_memory_search`. The source
/// contract is the fixture of truth; see the `phase1_contract` tests in this
/// module for source-parity validation.
pub fn validate_query(query: &str) -> Result<(), &'static str> {
    if query.is_empty() || query != query.trim() {
        return Err("query must be non-empty and have no surrounding whitespace");
    }
    let lower = query.to_ascii_lowercase();

    // Rule 1: declarative statement, not an interrogative question.
    if query.ends_with('?')
        || [
            "can ", "could ", "would ", "should ", "what ", "where ", "when ", "why ", "how ",
            "is ", "are ", "do ", "does ", "did ", "will ", "shall ", "may ", "might ", "has ",
            "have ", "had ", "am ", "was ", "were ", "who ", "whom ", "whose ",
        ]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
    {
        return Err("query must be declarative, not interrogative");
    }

    // Rules 1 & 4: declarative statement, omit retrieval-meta wording.
    // This is an intentionally bounded lexical guard, not a natural-language
    // classifier. It catches common leading request forms (including comparison
    // requests) while the prompt remains responsible for semantic completeness.
    // The objective canonical examples are checked separately below.
    const REQUEST_FORM_PREFIXES: &[&str] = &[
        "explain ",
        "describe ",
        "summarize ",
        "summarise ",
        "outline ",
        "detail ",
        "discuss ",
        "identify ",
        "clarify ",
        "demonstrate ",
        "enumerate ",
        "compare ",
        "recommend ",
        "suggest ",
    ];
    if REQUEST_FORM_PREFIXES
        .iter()
        .any(|prefix| lower.starts_with(prefix))
    {
        return Err("query contains retrieval-meta wording");
    }

    // The explicit Phase 1 contract examples are: `find`, `information about`,
    // and `search for`. Additional request forms are also retrieval-meta wording.
    const RETRIEVAL_META_PHRASES: &[&str] = &[
        "find",
        "information about",
        "search for",
        "look up",
        "retrieve",
        "tell me about",
        "tell me",
        "give me",
        "show me",
        "find me",
        "get me",
        "provide me",
        "list",
    ];
    if contains_phrase(&lower, RETRIEVAL_META_PHRASES) {
        return Err("query contains retrieval-meta wording");
    }

    // Rule 3: self-contained.
    if contains_word(
        &lower,
        &["this", "that", "these", "those", "it", "they", "them"],
    ) {
        return Err("query is not self-contained");
    }

    // Rule 2: one information need.
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

    fn valid_query() -> &'static str {
        "Database migration timeout E_CONNRESET"
    }

    fn payload_with_first_query(query: &str) -> String {
        format!(
            r#"{{"queries":[{{"type":"case","query":"{query}"}},{{"type":"case","query":"{valid}"}}]}}"#,
            valid = valid_query()
        )
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
    fn parser_rejects_malformed_json() {
        assert!(matches!(
            parse_planned_queries("not json"),
            Err(PlannerError::InvalidJson(_))
        ));
    }

    #[test]
    fn parser_rejects_imperative_explain() {
        let raw = payload_with_first_query("Explain migration timeout policy");
        let err = parse_planned_queries(&raw).unwrap_err();
        assert!(matches!(
            err,
            PlannerError::InvalidQuery {
                index: 0,
                reason: "query contains retrieval-meta wording"
            }
        ));
    }

    #[test]
    fn parser_rejects_imperative_compare() {
        let raw = payload_with_first_query("Compare migration timeout policy variants");
        let err = parse_planned_queries(&raw).unwrap_err();
        assert!(matches!(
            err,
            PlannerError::InvalidQuery {
                index: 0,
                reason: "query contains retrieval-meta wording"
            }
        ));
    }

    #[test]
    fn parser_rejects_unknown_note_type() {
        let raw = r#"{"queries":[{"type":"unknown","query":"Valid statement"},{"type":"case","query":"Another valid statement"}]}"#;
        assert!(parse_planned_queries(raw).is_err());
    }

    #[test]
    fn parser_rejects_too_few_queries() {
        let raw = r#"{"queries":[{"type":"case","query":"Only one"}]}"#;
        assert!(matches!(
            parse_planned_queries(raw),
            Err(PlannerError::WrongQueryCount(1))
        ));
    }

    #[test]
    fn parser_rejects_too_many_queries() {
        let raw = r#"{"queries":[{"type":"case","query":"One"},{"type":"case","query":"Two"},{"type":"case","query":"Three"},{"type":"case","query":"Four"},{"type":"case","query":"Five"}]}"#;
        assert!(matches!(
            parse_planned_queries(raw),
            Err(PlannerError::WrongQueryCount(5))
        ));
    }

    #[test]
    fn parser_rejects_empty_and_surrounding_whitespace_queries() {
        for query in [
            "",
            " Database migration timeout E_CONNRESET",
            "Database migration timeout E_CONNRESET ",
        ] {
            let raw = payload_with_first_query(query);
            assert_eq!(
                parse_planned_queries(&raw),
                Err(PlannerError::InvalidQuery {
                    index: 0,
                    reason: "query must be non-empty and have no surrounding whitespace",
                }),
                "query should be rejected: {query:?}"
            );
        }
    }

    #[test]
    fn parser_rejects_trailing_question_mark_and_interrogative_prefix() {
        for query in [
            "Migration timeout policy?",
            "How migration timeout policy works",
        ] {
            let raw = payload_with_first_query(query);
            assert_eq!(
                parse_planned_queries(&raw),
                Err(PlannerError::InvalidQuery {
                    index: 0,
                    reason: "query must be declarative, not interrogative",
                }),
                "query should be rejected: {query}"
            );
        }
    }

    #[test]
    fn parser_rejects_retrieval_meta_find() {
        let raw = payload_with_first_query("Find migration failures");
        let err = parse_planned_queries(&raw).unwrap_err();
        assert!(matches!(
            err,
            PlannerError::InvalidQuery {
                index: 0,
                reason: "query contains retrieval-meta wording"
            }
        ));
    }

    #[test]
    fn parser_rejects_retrieval_meta_information_about() {
        let raw = payload_with_first_query("Information about migration failures");
        let err = parse_planned_queries(&raw).unwrap_err();
        assert!(matches!(
            err,
            PlannerError::InvalidQuery {
                index: 0,
                reason: "query contains retrieval-meta wording"
            }
        ));
    }

    #[test]
    fn parser_rejects_retrieval_meta_search_for() {
        let raw = payload_with_first_query("Search for migration failures");
        let err = parse_planned_queries(&raw).unwrap_err();
        assert!(matches!(
            err,
            PlannerError::InvalidQuery {
                index: 0,
                reason: "query contains retrieval-meta wording"
            }
        ));
    }

    #[test]
    fn parser_rejects_imperative_tell_me_about() {
        let raw = payload_with_first_query("Tell me about migration timeout policy");
        let err = parse_planned_queries(&raw).unwrap_err();
        assert!(matches!(
            err,
            PlannerError::InvalidQuery {
                index: 0,
                reason: "query contains retrieval-meta wording"
            }
        ));
    }

    #[test]
    fn parser_rejects_each_documented_deictic_pronoun() {
        for pronoun in ["this", "that", "these", "those", "it", "they", "them"] {
            let raw = payload_with_first_query(&format!("Migration timeout policy for {pronoun}"));
            assert_eq!(
                parse_planned_queries(&raw),
                Err(PlannerError::InvalidQuery {
                    index: 0,
                    reason: "query is not self-contained",
                }),
                "pronoun should be rejected: {pronoun}"
            );
        }
    }

    #[test]
    fn parser_rejects_each_documented_multi_need_delimiter() {
        for delimiter in ["; ", " and ", " or ", " versus "] {
            let raw = payload_with_first_query(&format!(
                "Migration timeout policy{delimiter}retry policy"
            ));
            assert_eq!(
                parse_planned_queries(&raw),
                Err(PlannerError::InvalidQuery {
                    index: 0,
                    reason: "query must express one information need",
                }),
                "delimiter should be rejected: {delimiter:?}"
            );
        }
    }

    /// Source-parity tests against the Phase 1 memory_search contract in
    /// `djinn-mcp-extension/src/shared_schemas.rs`. These tests include the
    /// source file and mechanically verify that the exact good/bad examples and
    /// prohibited retrieval-meta phrases are validated consistently by this
    /// local predicate, without adding a cross-crate dependency.
    mod phase1_contract {
        use super::*;

        const SOURCE: &str =
            include_str!("../../../../../../crates/djinn-mcp-extension/src/shared_schemas.rs");

        fn extract_backtick_quoted(source: &str, prefix: &str) -> Option<String> {
            let start = source.find(prefix)? + prefix.len();
            let end = source[start..].find('`')? + start;
            Some(source[start..end].to_string())
        }

        fn extract_retrieval_meta_phrases(source: &str) -> Vec<String> {
            let prefix = "omit retrieval-meta wording such as ";
            let start = source
                .find(prefix)
                .expect("source should mention retrieval-meta wording")
                + prefix.len();
            let rest = &source[start..];
            let end = rest.find('.').unwrap_or(rest.len());
            let clause = &rest[..end];

            let mut phrases = Vec::new();
            let mut offset = 0;
            while let Some(open) = clause[offset..].find('`') {
                let open = offset + open;
                let close = clause[open + 1..].find('`').expect("unmatched backtick") + open + 1;
                phrases.push(clause[open + 1..close].to_string());
                offset = close + 1;
            }
            phrases
        }

        #[test]
        fn source_contract_contains_expected_clauses() {
            for required in [
                "write a declarative statement, not an interrogative question",
                "express one information need per query",
                "make each query self-contained",
                "omit retrieval-meta wording such as `find`, `information about`, and `search for`",
                "preserve discriminative symbol names, exact error strings, and config keys verbatim",
            ] {
                assert!(
                    SOURCE.contains(required),
                    "source contract should mention `{required}`"
                );
            }
        }

        #[test]
        fn source_declarative_rule_rejects_unlisted_request_forms() {
            // "Such as" in the source contract makes the quoted retrieval-meta
            // phrases examples, not an exhaustive allow-list. These forms would
            // otherwise pass the interrogative and exact-example checks while
            // violating its required declarative statement form.
            for request in [
                "Tell me about migration timeout policy",
                "Explain migration timeout policy",
                "Compare migration timeout policy variants",
            ] {
                assert!(
                    validate_query(request).is_err(),
                    "request-form query should violate the source declarative rule: {request}"
                );
            }
        }

        #[test]
        fn source_good_example_is_accepted() {
            let good = extract_backtick_quoted(SOURCE, "Good query: `").expect("good example");
            assert_eq!(good, "Authentication timeout handling for E_CONNRESET");
            assert!(
                validate_query(&good).is_ok(),
                "good example should be accepted: {good}"
            );
        }

        #[test]
        fn source_bad_example_is_rejected() {
            let bad = extract_backtick_quoted(SOURCE, "Bad query: `").expect("bad example");
            assert_eq!(
                bad,
                "Can you find information about authentication timeout errors?"
            );
            assert!(
                validate_query(&bad).is_err(),
                "bad example should be rejected: {bad}"
            );
        }

        #[test]
        fn source_retrieval_meta_phrases_are_rejected() {
            let phrases = extract_retrieval_meta_phrases(SOURCE);
            assert_eq!(phrases, vec!["find", "information about", "search for"]);
            for phrase in phrases {
                let query = format!("{phrase} migration failures");
                assert!(
                    validate_query(&query).is_err(),
                    "retrieval-meta phrase `{phrase}` should be rejected in query `{query}`"
                );
            }
        }
    }
}
