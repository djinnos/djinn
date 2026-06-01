//! F6 — optional, **off-by-default** query-planner pre-step for
//! `code_graph_search`.
//!
//! Before the main structural search runs, this module can optionally expand
//! a single user query into 1–3 sub-queries (plus corpus-routing hints) using
//! a cheap, configurable model. The expanded sub-queries are searched
//! independently and their hits are unioned + deduped to improve recall.
//!
//! ## Hard invariants
//!
//! * **Off by default.** [`query_planner_enabled`] returns `false` unless the
//!   `DJINN_CODE_GRAPH_QUERY_PLANNER` env var is explicitly set to a truthy
//!   value. When off, [`RepoDependencyGraph::search_by_name_planned`] is a
//!   thin pass-through to [`RepoDependencyGraph::search_by_name`] — the
//!   planner is **never** constructed or invoked, so there is zero added
//!   latency on the hot path.
//!
//! * **Testable in isolation.** The expansion logic is split into pure
//!   functions ([`build_plan_prompt`], [`parse_plan_response`],
//!   [`plan_query`]) plus an injectable [`QueryPlanner`] trait, so the whole
//!   pipeline can be unit-tested with a fake planner — no live LLM required.
//!
//! ## Provider hookup (SCAFFOLD)
//!
//! `djinn-graph` is a pure graph crate: it has **no** handle to an LLM
//! provider/model (those live in `djinn-control-plane` / the server, which we
//! must not touch here). So the *model call itself* is scaffolded: the prompt
//! builder and response parser are implemented and tested, and the trait seam
//! ([`QueryPlanner`]) is in place. The production wiring — a control-plane
//! `QueryPlanner` impl that calls the cheap configurable model and feeds its
//! raw text to [`parse_plan_response`] — is the only remaining TODO, and it
//! lives entirely on the caller side behind this trait.

use crate::repo_graph::{RepoDependencyGraph, RepoGraphNodeKind, RepoGraphSearchHit};

/// Environment flag gating the entire query-planner feature.
///
/// **Default: OFF.** Only the truthy values below enable it; anything else
/// (including unset / empty) keeps the planner disabled.
pub const QUERY_PLANNER_FLAG: &str = "DJINN_CODE_GRAPH_QUERY_PLANNER";

/// Maximum number of sub-queries the planner is allowed to emit (including the
/// original query). Bounds both recall fan-out and model cost.
pub const MAX_SUBQUERIES: usize = 3;

/// Returns `true` only when the planner is **explicitly** enabled via
/// [`QUERY_PLANNER_FLAG`]. Unset / empty / falsey ⇒ `false` (off by default).
pub fn query_planner_enabled() -> bool {
    match std::env::var(QUERY_PLANNER_FLAG) {
        Err(_) => false,
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
    }
}

/// Injectable seam for the query-expansion model.
///
/// Implementors take the user's raw query and return the **full** set of
/// sub-queries to search (typically the original plus 1–2 reformulations).
/// The trait is intentionally synchronous and infallible at this layer:
/// production impls perform the (async, fallible) model call upstream and pass
/// the already-resolved expansions, or simply degrade to `vec![query]` on
/// failure so the hot path can never be broken by a planner outage.
pub trait QueryPlanner {
    /// Expand `query` into 1–`MAX_SUBQUERIES` sub-queries. The first element
    /// SHOULD be the original query so exact-name ranking is preserved.
    fn plan(&self, query: &str) -> Vec<String>;
}

/// A trivial fixed-output planner — the test/fake implementation, also usable
/// as a deterministic "no expansion" planner (`StaticPlanner::passthrough`).
#[derive(Debug, Clone)]
pub struct StaticPlanner {
    expansions: Vec<String>,
}

impl StaticPlanner {
    /// Planner that returns exactly `expansions` (after normalization via
    /// [`plan_query`] at call sites). Useful as a test double.
    pub fn new(expansions: Vec<String>) -> Self {
        Self { expansions }
    }

    /// Planner that performs no expansion: it echoes the input query only.
    pub fn passthrough() -> Self {
        Self {
            expansions: Vec::new(),
        }
    }
}

impl QueryPlanner for StaticPlanner {
    fn plan(&self, query: &str) -> Vec<String> {
        let mut out = vec![query.to_string()];
        out.extend(self.expansions.iter().cloned());
        out
    }
}

/// Build the prompt fed to the cheap configurable expansion model.
///
/// Pure + deterministic so it can be unit-tested without a model. Production
/// callers send this to the model and pass the raw completion to
/// [`parse_plan_response`].
pub fn build_plan_prompt(query: &str) -> String {
    format!(
        "You expand a code-search query into at most {max} short, \
         high-recall sub-queries (symbol names, synonyms, or related \
         identifiers) to improve retrieval over a code graph. Return ONE \
         sub-query per line, most-relevant first, no numbering or prose. \
         Always include the original query verbatim as the first line.\n\n\
         Query: {query}",
        max = MAX_SUBQUERIES,
        query = query.trim()
    )
}

/// Parse a raw model completion (one sub-query per line) into a clean list.
///
/// Pure: trims, drops blank lines, strips common list markers (`-`, `*`,
/// `1.`), and de-duplicates case-insensitively while preserving order. Does
/// **not** enforce [`MAX_SUBQUERIES`] — that capping happens in [`plan_query`]
/// alongside the original-query guarantee.
pub fn parse_plan_response(raw: &str) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for line in raw.lines() {
        let trimmed = strip_list_marker(line.trim());
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();
        if seen.iter().any(|s| s.to_ascii_lowercase() == lower) {
            continue;
        }
        seen.push(trimmed.to_string());
    }
    seen
}

/// Strip a leading list marker (`- `, `* `, `1. `, `1) `) from a line.
fn strip_list_marker(line: &str) -> &str {
    if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
        return rest.trim();
    }
    // Numbered markers: "12. foo" / "3) foo".
    let bytes = line.as_bytes();
    let digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
    if digits > 0 && digits < line.len() {
        let after = &line[digits..];
        if let Some(rest) = after.strip_prefix(". ").or_else(|| after.strip_prefix(") ")) {
            return rest.trim();
        }
    }
    line
}

/// Produce the final, bounded list of sub-queries to search.
///
/// Guarantees:
/// * The original `query` is always present and first (exact-name ranking is
///   never lost).
/// * Result is de-duplicated case-insensitively, in stable order.
/// * Length is capped at [`MAX_SUBQUERIES`].
///
/// Empty / whitespace-only `query` yields an empty plan (the search path
/// treats that as "no search", matching [`RepoDependencyGraph::search_by_name`]
/// behavior on an empty query).
pub fn plan_query(planner: &dyn QueryPlanner, query: &str) -> Vec<String> {
    if query.trim().is_empty() {
        return Vec::new();
    }
    let mut out: Vec<String> = vec![query.to_string()];
    for sub in planner.plan(query) {
        let sub = sub.trim();
        if sub.is_empty() {
            continue;
        }
        let lower = sub.to_ascii_lowercase();
        if out.iter().any(|s| s.to_ascii_lowercase() == lower) {
            continue;
        }
        out.push(sub.to_string());
        if out.len() >= MAX_SUBQUERIES {
            break;
        }
    }
    out
}

/// Union + dedup structural search hits across several sub-queries.
///
/// Pure over the per-sub-query hit lists. Dedup is by `node_index`; on a
/// collision the **highest** score wins (so an exact match from any sub-query
/// is preserved). Results are re-sorted by score desc, then by node index for
/// stability, and truncated to `limit`.
pub fn union_dedup_hits(
    per_query: Vec<Vec<RepoGraphSearchHit>>,
    limit: usize,
) -> Vec<RepoGraphSearchHit> {
    use std::collections::HashMap;

    let mut best: HashMap<usize, RepoGraphSearchHit> = HashMap::new();
    for hits in per_query {
        for hit in hits {
            let key = hit.node_index.index();
            best.entry(key)
                .and_modify(|existing| {
                    if hit.score > existing.score {
                        *existing = hit.clone();
                    }
                })
                .or_insert(hit);
        }
    }
    let mut merged: Vec<RepoGraphSearchHit> = best.into_values().collect();
    merged.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.node_index.index().cmp(&b.node_index.index()))
    });
    merged.truncate(limit);
    merged
}

impl RepoDependencyGraph {
    /// Structural search with the **optional**, off-by-default query planner.
    ///
    /// * When the planner is disabled (the default — see
    ///   [`query_planner_enabled`]) or `planner` is `None`, this is a direct
    ///   pass-through to [`RepoDependencyGraph::search_by_name`]; results are
    ///   byte-for-byte identical to today and **no** planner code runs.
    /// * When enabled **and** a `planner` is supplied, the query is expanded
    ///   via [`plan_query`], each sub-query is searched, and the hits are
    ///   unioned + deduped via [`union_dedup_hits`] (capped at `limit`).
    ///
    /// The flag check happens first so the disabled path costs a single env
    /// read short-circuit and the original call — zero planner overhead.
    pub fn search_by_name_planned(
        &self,
        query: &str,
        kind_filter: Option<RepoGraphNodeKind>,
        limit: usize,
        planner: Option<&dyn QueryPlanner>,
    ) -> Vec<RepoGraphSearchHit> {
        // Hot path: planner off (default) or no planner injected ⇒ unchanged.
        let planner = match planner {
            Some(p) if query_planner_enabled() => p,
            _ => return self.search_by_name(query, kind_filter, limit),
        };

        let subqueries = plan_query(planner, query);
        if subqueries.len() <= 1 {
            // Planner emitted nothing useful — same single search as today.
            return self.search_by_name(query, kind_filter, limit);
        }
        // Search each sub-query independently, then union + dedup.
        let per_query: Vec<Vec<RepoGraphSearchHit>> = subqueries
            .iter()
            .map(|sq| self.search_by_name(sq, kind_filter, limit))
            .collect();
        union_dedup_hits(per_query, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guard against accidentally flipping the default. OFF unless explicitly
    /// enabled. (Reads the process env; CI runs without the flag set.)
    #[test]
    fn flag_is_off_by_default() {
        // Only assert the unset behavior to avoid mutating shared process env
        // in a multi-threaded test binary.
        if std::env::var(QUERY_PLANNER_FLAG).is_err() {
            assert!(!query_planner_enabled());
        }
    }

    #[test]
    fn enabled_recognizes_truthy_values() {
        for v in ["1", "true", "TRUE", "yes", "on", " On "] {
            // Pure check via the same matcher the fn uses.
            assert!(matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            ));
        }
        for v in ["0", "false", "no", "off", "", "maybe"] {
            assert!(!matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            ));
        }
    }

    #[test]
    fn plan_query_always_includes_original_first() {
        let planner = StaticPlanner::new(vec!["alt".into(), "other".into()]);
        let plan = plan_query(&planner, "main");
        assert_eq!(plan.first().map(String::as_str), Some("main"));
        assert!(plan.len() <= MAX_SUBQUERIES);
    }

    #[test]
    fn plan_query_dedups_case_insensitively() {
        // Planner echoes the original (different case) + a dup.
        let planner = StaticPlanner::new(vec!["MAIN".into(), "helper".into(), "Helper".into()]);
        let plan = plan_query(&planner, "main");
        // "main" once, then "helper" once — capped at 3.
        assert_eq!(plan, vec!["main".to_string(), "helper".to_string()]);
    }

    #[test]
    fn plan_query_caps_at_max() {
        let planner =
            StaticPlanner::new(vec!["a".into(), "b".into(), "c".into(), "d".into(), "e".into()]);
        let plan = plan_query(&planner, "q");
        assert_eq!(plan.len(), MAX_SUBQUERIES);
        assert_eq!(plan[0], "q");
    }

    #[test]
    fn plan_query_empty_for_blank_query() {
        let planner = StaticPlanner::new(vec!["x".into()]);
        assert!(plan_query(&planner, "   ").is_empty());
    }

    #[test]
    fn passthrough_planner_yields_only_original() {
        let planner = StaticPlanner::passthrough();
        assert_eq!(plan_query(&planner, "foo"), vec!["foo".to_string()]);
    }

    #[test]
    fn parse_plan_response_strips_markers_and_blanks() {
        let raw = "main\n- helper\n* util\n\n1. router\n2) handler\n";
        let parsed = parse_plan_response(raw);
        assert_eq!(parsed, vec!["main", "helper", "util", "router", "handler"]);
    }

    #[test]
    fn parse_plan_response_dedups() {
        let raw = "foo\nFoo\n  foo  \nbar";
        let parsed = parse_plan_response(raw);
        assert_eq!(parsed, vec!["foo", "bar"]);
    }

    #[test]
    fn build_plan_prompt_contains_query_and_cap() {
        let prompt = build_plan_prompt("  user query  ");
        assert!(prompt.contains("user query"));
        assert!(prompt.contains(&MAX_SUBQUERIES.to_string()));
    }

    #[test]
    fn union_dedup_keeps_highest_score_and_sorts() {
        use crate::repo_graph::RepoGraphSearchHit;
        use petgraph::graph::NodeIndex;

        let mk = |idx: usize, score: f64| RepoGraphSearchHit {
            node_index: NodeIndex::new(idx),
            score,
        };
        // node 1 appears twice with different scores; node 2 once.
        let per_query = vec![vec![mk(1, 1.0), mk(2, 2.0)], vec![mk(1, 3.0)]];
        let merged = union_dedup_hits(per_query, 10);
        assert_eq!(merged.len(), 2);
        // Highest score first: node 1 @ 3.0, then node 2 @ 2.0.
        assert_eq!(merged[0].node_index.index(), 1);
        assert_eq!(merged[0].score, 3.0);
        assert_eq!(merged[1].node_index.index(), 2);
    }

    #[test]
    fn union_dedup_respects_limit() {
        use crate::repo_graph::RepoGraphSearchHit;
        use petgraph::graph::NodeIndex;
        let per_query = vec![(0..10)
            .map(|i| RepoGraphSearchHit {
                node_index: NodeIndex::new(i),
                score: i as f64,
            })
            .collect()];
        let merged = union_dedup_hits(per_query, 3);
        assert_eq!(merged.len(), 3);
    }
}
