//! df6s: shared slicing/counting helpers for the agent-boundary
//! pagination work on `neighbors` / `impact` / `coupling_hotspots`.
//!
//! The `code_graph` traversal ops all share the same contract at the
//! MCP/chat/agent boundary: the underlying `RepoDependencyGraph`
//! traversal always runs to completion, then we apply `offset` and
//! `limit` only when constructing the response DTO. The total
//! count the response ships always reflects the unsliced post-exclusion
//! set, so a paginated caller can never mistake "page 3 was empty" for
//! "the graph has no more neighbors".
//!
//! These helpers live here (not in `request_types` or `response_types`)
//! because they bridge the two: a `CodeGraphParams` describes what
//! the caller wants, a `Response` DTO describes what the model sees,
//! and the helpers do the slicing + counting in between.

use std::collections::BTreeMap;

use super::request_types::PaginationParams;

/// Result of slicing an unsliced list at the response DTO layer.
/// `vec` is mutated in place (drained) so the caller can drop the
/// consumed tail without reallocating; `has_more` indicates whether
/// another page follows the slice we just kept.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PageSlice {
    /// True when the unsliced list ran past `offset + limit`.
    pub has_more: bool,
}

/// df6s: drain an unsliced list down to the `(offset, limit)` page,
/// mutating it in place. Returns whether another page remains.
///
/// The math is `min(unsliced, offset + limit)`; we then drop the
/// first `offset` entries (slice via `drain` to avoid reallocating
/// the suffix). `offset_past_end` returns `has_more = false` and
/// leaves the vector empty — the "asked too far" case is a real
/// end-of-results, not a bug.
pub(crate) fn apply_page_slice<T>(vec: &mut Vec<T>, offset: usize, limit: usize) -> PageSlice {
    let total = vec.len();
    if offset >= total {
        vec.clear();
        return PageSlice { has_more: false };
    }
    // Drain the prefix the caller skipped past.
    if offset > 0 {
        vec.drain(0..offset);
    }
    let kept = vec.len();
    if kept > limit {
        vec.truncate(limit);
        PageSlice { has_more: true }
    } else {
        PageSlice { has_more: false }
    }
}

/// df6s: decide whether the response DTO should ship pagination
/// metadata. The contract: emit `total` / `offset` / `limit` /
/// `has_more` when ANY of the following hold:
///
/// - `offset > 0` — the caller paginated past the first page.
/// - `summary_only` is `true` — the counts-only mode ships `total`
///   as the count signal.
/// - `total > page_limit` — the underlying result outgrew the
///   cap, so a `has_more = true` is a real signal even when offset
///   is zero.
///
/// A caller that asked for the default page (offset=0, no
/// `summary_only`) and got a short result shouldn't see
/// `has_more: false` — that's just noise on the already-familiar
/// full-page shape. `page_limit` is the resolved per-op cap (e.g.
/// 20 for `neighbors`, 100 for `impact`/`coupling_hotspots`).
pub(crate) fn pagination_applied(
    pagination: PaginationParams,
    total: usize,
    page_limit: usize,
) -> bool {
    pagination.offset > 0 || pagination.summary_only || total > page_limit
}

/// df6s: build the per-depth count map for an `impact` detailed set.
/// Returns a `BTreeMap<String, usize>` keyed by stringified depth so
/// the JSON serialization keeps the natural numeric ordering (BTree
//  → 1, 2, 3, …) on the wire. Buckets with zero entries are dropped
/// so the response only shows depths that actually appeared.
///
/// Pure function: callers should pass the **unsliced** `Vec<ImpactEntry>`
/// (the one we built the risk metrics from) so the breakdown reflects
/// the full blast-radius distribution, not the page.
pub(crate) fn build_by_depth_counts(
    entries: &[crate::bridge::ImpactEntry],
) -> BTreeMap<String, usize> {
    let mut counts: BTreeMap<usize, usize> = BTreeMap::new();
    for entry in entries {
        *counts.entry(entry.depth).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .map(|(depth, count)| (depth.to_string(), count))
        .collect()
}
