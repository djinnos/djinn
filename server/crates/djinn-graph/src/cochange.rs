//! Proposal qoxm: commit co-change coupling materialized as graph edges.
//!
//! Djinn already ingests commit-based file coupling into the
//! `coupling_pair_events` table (see [`crate::coupling_index`]). This module
//! turns the aggregated file-pair co-change counts into first-class
//! [`RepoGraphEdgeKind::CoChangedWith`] edges so the galaxy view can render
//! "these files change together," the `edges` op can surface them, and
//! `refactor_candidates` can weight cross-module coupling.
//!
//! Storage note: co-change edges are deliberately kept in a dedicated sidecar
//! ([`RepoDependencyGraph::cochange_edges`]) OUTSIDE the PageRank/traversal
//! petgraph. Co-change is circumstantial history, not a structural dependency;
//! folding it into the petgraph would silently inflate PageRank, node degree,
//! SCC/cycle detection, and impact blast radii. Keeping it in a sidecar makes
//! "excluded by default" free for every traversal/ranking op — the edges are
//! opt-in via the `edges` op's edge-kind filter and are surfaced by `snapshot`
//! as a distinct visual channel. They still round-trip through the artifact via
//! the existing `edges` vec (partitioned back into the sidecar on load).

use petgraph::graph::NodeIndex;

use crate::repo_graph::RepoDependencyGraph;

/// Minimum distinct co-change commit count for a pair to become an edge.
/// Chosen so the resulting coupling score sits at the ~0.3 floor (see
/// [`coupling_score`]): two files that changed together only once or twice are
/// noise, not signal.
pub const COCHANGE_MIN_CO_CHANGES: usize = 3;

/// Saturating half-constant `K` in `score = n / (n + K)`. `K = 7` maps the
/// [`COCHANGE_MIN_CO_CHANGES`] floor of 3 co-changes to a score of 0.30, 7
/// co-changes to 0.50 (the Inferred/Ambiguous tier boundary), and asymptotes
/// toward 1.0 for heavily-coupled pairs.
pub const COCHANGE_SCORE_K: f64 = 7.0;

/// Minimum coupling score to keep a co-change edge. Mirrors the count floor
/// via [`coupling_score`]`(COCHANGE_MIN_CO_CHANGES)`.
pub const COCHANGE_SCORE_FLOOR: f64 = 0.30;

/// Per-file partner cap. Each file keeps at most this many of its
/// highest-scoring co-change partners, bounding total edge count to roughly
/// `files * K / 2` regardless of how noisy the commit history is.
pub const COCHANGE_TOP_K_PER_FILE: usize = 8;

/// Upper bound on file pairs fetched from the coupling index per warm. Bounds
/// the DB read and the edge-derivation work on pathological histories.
pub const COCHANGE_MAX_PAIRS: usize = 20_000;

/// Reason-string prefix stamped on the artifact edge so the temporal
/// `last_co_change` epoch day survives the artifact round-trip (bincode is
/// positional; reusing the existing `reason` field avoids an additive artifact
/// struct change). Shape: `"cochange;last_day=<i64>"`.
pub const COCHANGE_REASON_PREFIX: &str = "cochange;last_day=";

/// A materialized co-change coupling edge, keyed by graph [`NodeIndex`] (like
/// [`crate::repo_graph`]'s symbol-range sidecar). Undirected — stored once with
/// `source`/`target` in the canonical `file_a < file_b` order the coupling
/// index emits.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CoChangeEdge {
    pub source: NodeIndex,
    pub target: NodeIndex,
    /// Distinct commit count in which both files changed together.
    pub evidence_count: usize,
    /// Coupling score in `[0, 1]` — see [`coupling_score`].
    pub confidence: f64,
    /// Epoch day (days since 1970-01-01) of the most recent co-change.
    pub last_co_change: i64,
}

/// Plain-data input row derived from the coupling index (see
/// `djinn_db::CommitFileChangeRepository::top_coupled_pairs`). Kept string-only
/// so the async DB read can hand a `Vec` across the `spawn_blocking` boundary
/// into the synchronous graph-build closure.
#[derive(Debug, Clone)]
pub struct CoChangeInput {
    pub file_a: String,
    pub file_b: String,
    pub co_changes: usize,
    /// ISO-8601 timestamp of the most recent co-change commit.
    pub last_co_change_iso: String,
}

/// Coupling score for a co-change count: a bounded, monotonic saturating
/// transform `n / (n + K)`. Higher counts asymptote toward 1.0 without ever
/// reaching it, so a co-change edge can never masquerade as proof.
pub fn coupling_score(co_changes: usize) -> f64 {
    let n = co_changes as f64;
    n / (n + COCHANGE_SCORE_K)
}

/// Build the co-change reason string carrying the temporal property.
pub fn encode_reason(last_co_change: i64) -> String {
    format!("{COCHANGE_REASON_PREFIX}{last_co_change}")
}

/// Recover the `last_co_change` epoch day from a co-change edge reason string.
/// Returns 0 when the marker is absent or unparseable (defensive — legacy or
/// hand-authored edges).
pub fn decode_last_co_change(reason: Option<&str>) -> i64 {
    reason
        .and_then(|r| r.strip_prefix(COCHANGE_REASON_PREFIX))
        .and_then(|d| d.trim().parse::<i64>().ok())
        .unwrap_or(0)
}

/// Days from a proleptic-Gregorian civil date to 1970-01-01 (Howard Hinnant's
/// `days_from_civil`). Avoids pulling a date library for a one-off conversion.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Parse the leading `YYYY-MM-DD` of an ISO-8601 timestamp into an epoch day.
/// Returns 0 on any parse failure (the temporal property is best-effort).
pub fn epoch_day_from_iso(iso: &str) -> i64 {
    let date = iso.get(0..10).unwrap_or("");
    let mut parts = date.split('-');
    let y = parts.next().and_then(|s| s.parse::<i64>().ok());
    let m = parts.next().and_then(|s| s.parse::<i64>().ok());
    let d = parts.next().and_then(|s| s.parse::<i64>().ok());
    match (y, m, d) {
        (Some(y), Some(m), Some(d)) if (1..=12).contains(&m) && (1..=31).contains(&d) => {
            days_from_civil(y, m, d)
        }
        _ => 0,
    }
}

/// Materialize co-change edges from coupling-index pair rows against the graph.
///
/// * Drops pairs below [`COCHANGE_MIN_CO_CHANGES`] / [`COCHANGE_SCORE_FLOOR`].
/// * Drops pairs whose files are not both present as file nodes in the graph
///   (e.g. deleted files, or files SCIP never indexed).
/// * Applies a per-file top-K partner cap: edges are considered highest-score
///   first and kept only while neither endpoint has hit
///   [`COCHANGE_TOP_K_PER_FILE`], bounding total edge count.
///
/// Returned edges are sorted deterministically (by endpoint index) so the
/// artifact serialization is stable across warms.
pub fn derive_cochange_edges(
    graph: &RepoDependencyGraph,
    inputs: &[CoChangeInput],
) -> Vec<CoChangeEdge> {
    use std::collections::HashMap;
    use std::path::Path;

    // Resolve + filter into candidate edges, highest score first.
    let mut candidates: Vec<CoChangeEdge> = Vec::new();
    for input in inputs {
        if input.co_changes < COCHANGE_MIN_CO_CHANGES {
            continue;
        }
        let score = coupling_score(input.co_changes);
        if score + f64::EPSILON < COCHANGE_SCORE_FLOOR {
            continue;
        }
        let (Some(a), Some(b)) = (
            graph.file_node(Path::new(&input.file_a)),
            graph.file_node(Path::new(&input.file_b)),
        ) else {
            continue;
        };
        if a == b {
            continue;
        }
        candidates.push(CoChangeEdge {
            source: a,
            target: b,
            evidence_count: input.co_changes,
            confidence: score,
            last_co_change: epoch_day_from_iso(&input.last_co_change_iso),
        });
    }

    // Highest score first; deterministic tie-break on endpoint indices.
    candidates.sort_by(|x, y| {
        y.confidence
            .partial_cmp(&x.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| x.source.index().cmp(&y.source.index()))
            .then_with(|| x.target.index().cmp(&y.target.index()))
    });

    // Per-file partner cap.
    let mut partners: HashMap<NodeIndex, usize> = HashMap::new();
    let mut kept: Vec<CoChangeEdge> = Vec::new();
    for edge in candidates {
        let sa = partners.get(&edge.source).copied().unwrap_or(0);
        let sb = partners.get(&edge.target).copied().unwrap_or(0);
        if sa >= COCHANGE_TOP_K_PER_FILE || sb >= COCHANGE_TOP_K_PER_FILE {
            continue;
        }
        *partners.entry(edge.source).or_insert(0) += 1;
        *partners.entry(edge.target).or_insert(0) += 1;
        kept.push(edge);
    }

    kept.sort_by(|x, y| {
        x.source
            .index()
            .cmp(&y.source.index())
            .then_with(|| x.target.index().cmp(&y.target.index()))
    });
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coupling_score_hits_floor_and_band_boundary() {
        assert!((coupling_score(COCHANGE_MIN_CO_CHANGES) - 0.30).abs() < 1e-9);
        assert!((coupling_score(7) - 0.50).abs() < 1e-9);
        assert!(coupling_score(100) < 1.0);
        assert!(coupling_score(100) > coupling_score(10));
    }

    #[test]
    fn epoch_day_round_trips_through_reason() {
        // 2026-07-15 is a fixed reference; recompute via the same algorithm.
        let day = epoch_day_from_iso("2026-07-15T12:00:00.000Z");
        assert_eq!(day, days_from_civil(2026, 7, 15));
        assert!(day > 20_000);
        let reason = encode_reason(day);
        assert_eq!(decode_last_co_change(Some(&reason)), day);
        assert_eq!(decode_last_co_change(Some("local-prefix")), 0);
        assert_eq!(decode_last_co_change(None), 0);
    }

    #[test]
    fn epoch_day_is_zero_on_garbage() {
        assert_eq!(epoch_day_from_iso(""), 0);
        assert_eq!(epoch_day_from_iso("not-a-date"), 0);
        assert_eq!(epoch_day_from_iso("2026-13-40T00:00:00Z"), 0);
    }
}
