/**
 * codeGraphLabels — pure helpers that decide which nodes should render
 * a label at the current camera zoom level, weighted by the node's
 * PageRank percentile within the active snapshot.
 *
 * The module is intentionally dependency-free (no React, no graphology,
 * no Sigma). The reducer/hook tasks compose these helpers; this file
 * only ships the algorithm.
 *
 * Conventions:
 *
 *  - `computePagerankPercentiles` mirrors `computeComplexityThresholds` in
 *    `codeGraphReducers.ts`: linear-interpolation percentile (Excel
 *    `PERCENTILE.INC` / `numpy.percentile`) with ties collapsed to the
 *    average rank.
 *  - `shouldLabelAtZoom` is a piecewise-linear ramp between two zoom
 *    breakpoints (named `ZOOM_FAR` / `ZOOM_NEAR`) and the top-percent
 *    threshold (`TOP_PERCENT_FAR`). Sentinel values (`null`, `NaN`,
 *    `Infinity`) fall through to a safe default rather than crashing the
 *    per-frame reducer.
 *  - The helpers are NOT memoized. The caller (the `useGraphReducers`
 *    hook) memoizes on graph identity / camera ratio.
 */

/**
 * Loose structural type that mirrors the surface area of a graphology
 * graph needed by the percentile helper. Defined here, in-place, so the
 * new module stays dependency-free — the test path can pass a hand-rolled
 * object without pulling in `graphology`.
 */
export interface PagerankGraphLike {
  nodes(): Iterable<string>;
  getNodeAttribute(id: string, key: string): unknown;
}

/**
 * The PageRank attribute key. Snapshots from `derive_graph_caches`
 * populate `node.pagerank` (numeric, finite). Anything else (missing,
 * non-numeric, NaN) is filtered out before ranking.
 */
const PAGERANK_ATTR = "pagerank";

/**
 * Compute each node's PageRank percentile in `[0, 1]` over the supplied
 * graph. Returns a `Map<nodeId, percentile>` keyed by node id.
 *
 * Semantics match `computeComplexityThresholds`:
 *
 *   1. Collect finite numeric PageRank values. Non-finite / missing
 *      values are excluded — those nodes will be absent from the result
 *      map and the downstream `shouldLabelAtZoom` will fall through to
 *      its `true` sentinel.
 *   2. Sort ascending.
 *   3. For each node, compute `percentile = rank / (n - 1)` where
 *      `rank` is the average index for ties — Excel `PERCENTILE.INC`
 *      style. The smallest value gets `0`, the largest gets `1`.
 *   4. Empty input → empty `Map`. No exceptions.
 */
export function computePagerankPercentiles(
  graph: PagerankGraphLike,
): Map<string, number> {
  const entries: Array<{ id: string; rank: number }> = [];

  for (const id of graph.nodes()) {
    const raw = graph.getNodeAttribute(id, PAGERANK_ATTR);
    if (typeof raw !== "number" || !Number.isFinite(raw)) continue;
    entries.push({ id, rank: raw });
  }

  const out = new Map<string, number>();
  const n = entries.length;
  if (n === 0) return out;

  // Sort ascending by PageRank so ties can be collapsed to the average
  // rank. `Array.prototype.sort` is stable in modern V8, so insertion
  // order is the tiebreaker — doesn't affect the average-rank result.
  entries.sort((a, b) => a.rank - b.rank);

  for (let i = 0; i < n; ) {
    let j = i;
    while (j + 1 < n && entries[j + 1].rank === entries[i].rank) j += 1;
    // Average rank in [0, 1] across the tied block. Dividing by (n - 1)
    // (not n) is the Excel `PERCENTILE.INC` / numpy default.
    const avgIndex = (i + j) / 2;
    const percentile = n === 1 ? 0 : avgIndex / (n - 1);
    for (let k = i; k <= j; k += 1) out.set(entries[k].id, percentile);
    i = j + 1;
  }

  return out;
}

/* ------------------------------------------------------------------ *
 * Zoom curve
 * ------------------------------------------------------------------ */

/**
 * Camera ratio at which the graph is "fully zoomed out". Sigma's
 * `getCamera().ratio` shrinks as the user zooms in (1 = fit-to-view,
 * <1 = zoomed in past the snapshot's natural extent, >1 = zoomed out).
 * At or below `ZOOM_FAR` we keep labels sparse.
 */
export const ZOOM_FAR = 0.05;

/**
 * Camera ratio at which the graph is "fully zoomed in". At or above
 * `ZOOM_NEAR` every node is eligible for a label.
 */
export const ZOOM_NEAR = 2.0;

/**
 * Fraction of nodes (by PageRank) that get a label at the far end of
 * the zoom curve. `TOP_PERCENT_FAR = 0.10` means the top 10% by
 * PageRank stay labeled when the user is fully zoomed out.
 */
export const TOP_PERCENT_FAR = 0.10;

/**
 * Should `node` render a label at the current zoom?
 *
 * The threshold percentile is a piecewise-linear ramp:
 *
 *   - `cameraRatio <= ZOOM_FAR` → `1 - TOP_PERCENT_FAR` (top N% only)
 *   - `cameraRatio >= ZOOM_NEAR` → `0` (all nodes)
 *   - in between → linear interpolation
 *
 * Sentinel handling:
 *
 *   - `percentile` is `null` / `undefined` / `NaN` → `true` (the node
 *     has no PageRank data; fall through to the existing LOD settings).
 *   - `cameraRatio` is `Infinity` → `true` (Sigma hasn't reported a
 *     camera yet; show labels so the first paint isn't empty).
 *   - `cameraRatio` is `NaN` / `-Infinity` → `false` (degenerate;
 *     conservative fallback that hides labels).
 *
 * `cameraRatio = 0` is finite and falls through to the curve's
 * far-zoom clamp (`1 - TOP_PERCENT_FAR`) — i.e. "treat as far-zoom".
 */
export function shouldLabelAtZoom(
  percentile: number | null | undefined,
  cameraRatio: number,
): boolean {
  if (percentile === null || percentile === undefined) return true;
  if (Number.isNaN(percentile)) return true;
  // `Infinity` here means "no camera reported yet"; show labels so the
  // first paint isn't empty. Any other non-finite ratio (NaN / -Inf) is
  // treated as the degenerate far-zoom case (`false`).
  if (cameraRatio === Infinity) return true;
  if (!Number.isFinite(cameraRatio)) return false;

  const threshold = labelThresholdForZoom(cameraRatio);
  return percentile >= threshold;
}

/**
 * The PageRank-percentile threshold above which a node should be
 * labeled at the given camera ratio. Exported for callers that want to
 * surface the curve in a debug overlay / slider.
 */
export function labelThresholdForZoom(cameraRatio: number): number {
  // Degenerate / far-zoom end of the curve.
  if (cameraRatio <= ZOOM_FAR) return 1 - TOP_PERCENT_FAR;
  // Fully zoomed in: no PageRank gating.
  if (cameraRatio >= ZOOM_NEAR) return 0;
  // Linear ramp between the two endpoints. `t` is the fraction of the
  // way from FAR → NEAR; the threshold drops from `1 - TOP_PERCENT_FAR`
  // toward `0`.
  const t = (cameraRatio - ZOOM_FAR) / (ZOOM_NEAR - ZOOM_FAR);
  return (1 - TOP_PERCENT_FAR) * (1 - t);
}
