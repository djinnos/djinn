import { describe, expect, it } from "vitest";

import {
  ZOOM_FAR,
  ZOOM_NEAR,
  TOP_PERCENT_FAR,
  computePagerankPercentiles,
  labelThresholdForZoom,
  shouldLabelAtZoom,
  type PagerankGraphLike,
} from "./codeGraphLabels";

/**
 * Hand-rolled graph stand-in. Keeps the percentile helper's tests off
 * the graphology dependency — same shape, plain Map storage.
 */
function makeGraph(
  nodeAttrs: Record<string, Record<string, unknown>>,
): PagerankGraphLike {
  return {
    nodes: function* () {
      for (const id of Object.keys(nodeAttrs)) yield id;
    },
    getNodeAttribute: (id, key) => nodeAttrs[id]?.[key],
  };
}

describe("computePagerankPercentiles", () => {
  it("matches Excel PERCENTILE.INC for 5 distinct values", () => {
    const graph = makeGraph({
      a: { pagerank: 0.1 },
      b: { pagerank: 0.2 },
      c: { pagerank: 0.3 },
      d: { pagerank: 0.4 },
      e: { pagerank: 0.5 },
    });

    const pcts = computePagerankPercentiles(graph);
    expect(pcts.size).toBe(5);
    expect(pcts.get("a")).toBe(0);
    expect(pcts.get("b")).toBe(0.25);
    expect(pcts.get("c")).toBe(0.5);
    expect(pcts.get("d")).toBe(0.75);
    expect(pcts.get("e")).toBe(1);
  });

  it("collapses ties to the average rank", () => {
    // 4 nodes, two pairs of ties. Sorted ascending, indices 0..3, n-1 = 3.
    //   n2/n3 (rank 0.2) tie at indices 0,1 → avg index 0.5 → 0.5/3 = 1/6
    //   n0/n1 (rank 0.4) tie at indices 2,3 → avg index 2.5 → 2.5/3 = 5/6
    const graph = makeGraph({
      n0: { pagerank: 0.4 },
      n1: { pagerank: 0.4 },
      n2: { pagerank: 0.2 },
      n3: { pagerank: 0.2 },
    });

    const pcts = computePagerankPercentiles(graph);
    expect(pcts.size).toBe(4);
    expect(pcts.get("n0")).toBeCloseTo(5 / 6, 12);
    expect(pcts.get("n1")).toBeCloseTo(5 / 6, 12);
    expect(pcts.get("n2")).toBeCloseTo(1 / 6, 12);
    expect(pcts.get("n3")).toBeCloseTo(1 / 6, 12);
  });

  it("ranks 4 distinct descending values in reverse insertion order", () => {
    // Mirrors the spec example `[0.4, 0.3, 0.2, 0.1]` — descending by
    // insertion but the percentile helper sorts ascending, so the ids
    // end up at positions 3, 2, 1, 0 → percentiles 1, 2/3, 1/3, 0.
    const graph = makeGraph({
      a: { pagerank: 0.4 },
      b: { pagerank: 0.3 },
      c: { pagerank: 0.2 },
      d: { pagerank: 0.1 },
    });

    const pcts = computePagerankPercentiles(graph);
    expect(pcts.size).toBe(4);
    expect(pcts.get("a")).toBe(1);
    expect(pcts.get("b")).toBeCloseTo(2 / 3, 12);
    expect(pcts.get("c")).toBeCloseTo(1 / 3, 12);
    expect(pcts.get("d")).toBe(0);
  });

  it("returns an empty Map for an empty graph", () => {
    const graph = makeGraph({});
    const pcts = computePagerankPercentiles(graph);
    expect(pcts).toBeInstanceOf(Map);
    expect(pcts.size).toBe(0);
  });

  it("handles a single node (degenerate rank = 0)", () => {
    const graph = makeGraph({ only: { pagerank: 0.42 } });
    const pcts = computePagerankPercentiles(graph);
    expect(pcts.size).toBe(1);
    expect(pcts.get("only")).toBe(0);
  });

  it("skips nodes with non-finite or missing PageRank", () => {
    const graph = makeGraph({
      a: { pagerank: 0.1 },
      b: {}, // missing
      c: { pagerank: NaN },
      d: { pagerank: "0.5" }, // wrong type
      e: { pagerank: 0.3 },
      f: { pagerank: Infinity },
    });

    const pcts = computePagerankPercentiles(graph);
    expect(pcts.size).toBe(2);
    expect(pcts.get("a")).toBe(0);
    expect(pcts.get("e")).toBe(1);
  });
});

describe("labelThresholdForZoom", () => {
  it("returns 1 - TOP_PERCENT_FAR at or below ZOOM_FAR", () => {
    expect(labelThresholdForZoom(0)).toBe(1 - TOP_PERCENT_FAR);
    expect(labelThresholdForZoom(ZOOM_FAR)).toBe(1 - TOP_PERCENT_FAR);
    // Just below the breakpoint — still on the far end of the curve.
    expect(labelThresholdForZoom(ZOOM_FAR - 0.01)).toBe(1 - TOP_PERCENT_FAR);
  });

  it("returns 0 at or above ZOOM_NEAR", () => {
    expect(labelThresholdForZoom(ZOOM_NEAR)).toBe(0);
    expect(labelThresholdForZoom(ZOOM_NEAR + 5)).toBe(0);
  });

  it("linearly interpolates between the two breakpoints", () => {
    const mid = (ZOOM_FAR + ZOOM_NEAR) / 2;
    const expected = (1 - TOP_PERCENT_FAR) * 0.5;
    expect(labelThresholdForZoom(mid)).toBeCloseTo(expected, 12);
  });
});

describe("shouldLabelAtZoom", () => {
  it("gates by PageRank percentile at ZOOM_FAR", () => {
    // At ZOOM_FAR the threshold is `1 - TOP_PERCENT_FAR = 0.90`, so the
    // 89th-percentile node falls off and the 91st-percentile node stays.
    expect(shouldLabelAtZoom(0.89, ZOOM_FAR)).toBe(false);
    expect(shouldLabelAtZoom(0.91, ZOOM_FAR)).toBe(true);
  });

  it("labels every node at ZOOM_NEAR (threshold = 0)", () => {
    expect(shouldLabelAtZoom(0.89, ZOOM_NEAR)).toBe(true);
    expect(shouldLabelAtZoom(0.91, ZOOM_NEAR)).toBe(true);
    expect(shouldLabelAtZoom(0, ZOOM_NEAR)).toBe(true);
  });

  it("interpolates the threshold at the midpoint zoom", () => {
    const mid = (ZOOM_FAR + ZOOM_NEAR) / 2;
    const threshold = labelThresholdForZoom(mid);
    // Just below the interpolated threshold → off; just above → on.
    expect(shouldLabelAtZoom(threshold - 0.01, mid)).toBe(false);
    expect(shouldLabelAtZoom(threshold + 0.01, mid)).toBe(true);
  });

  it("shows more labels as the camera zooms in", () => {
    // A mid-rank node (percentile = 0.5) flips from off at the far end
    // of the curve to on at the near end — the canonical "zoom in →
    // more labels" path the design wants.
    expect(shouldLabelAtZoom(0.5, ZOOM_FAR)).toBe(false);
    expect(shouldLabelAtZoom(0.5, ZOOM_NEAR)).toBe(true);
    // The threshold is monotonically non-increasing with zoom, so a
    // node that flips on at some midpoint stays on all the way to
    // ZOOM_NEAR.
    const mid = (ZOOM_FAR + ZOOM_NEAR) / 2;
    expect(shouldLabelAtZoom(0.5, mid)).toBe(true);
    expect(shouldLabelAtZoom(0.5, mid + 0.1)).toBe(true);
  });

  it("falls through (true) when percentile is null", () => {
    expect(shouldLabelAtZoom(null, ZOOM_FAR)).toBe(true);
    expect(shouldLabelAtZoom(null, ZOOM_NEAR)).toBe(true);
    expect(shouldLabelAtZoom(null, 0)).toBe(true);
  });

  it("falls through (true) when percentile is undefined", () => {
    expect(shouldLabelAtZoom(undefined, ZOOM_FAR)).toBe(true);
    expect(shouldLabelAtZoom(undefined, ZOOM_NEAR)).toBe(true);
  });

  it("falls through (true) when percentile is NaN", () => {
    expect(shouldLabelAtZoom(NaN, ZOOM_FAR)).toBe(true);
    expect(shouldLabelAtZoom(NaN, ZOOM_NEAR)).toBe(true);
  });

  it("falls through (true) when cameraRatio is Infinity", () => {
    // Sigma hasn't reported a camera yet; first paint shouldn't be empty.
    expect(shouldLabelAtZoom(0, Infinity)).toBe(true);
    expect(shouldLabelAtZoom(0.5, Infinity)).toBe(true);
    expect(shouldLabelAtZoom(1, Infinity)).toBe(true);
  });

  it("treats cameraRatio = 0 as far-zoom (degenerate)", () => {
    // Threshold at 0 is the same as ZOOM_FAR: 1 - TOP_PERCENT_FAR.
    // Below the threshold → off; above → on.
    const threshold = 1 - TOP_PERCENT_FAR;
    expect(shouldLabelAtZoom(threshold - 0.01, 0)).toBe(false);
    expect(shouldLabelAtZoom(threshold + 0.01, 0)).toBe(true);
  });

  it("returns false for non-finite, non-Infinity cameraRatio", () => {
    // NaN / -Infinity aren't valid camera states; the curve can't make
    // a meaningful call, so we lean conservative and hide labels.
    expect(shouldLabelAtZoom(0.5, NaN)).toBe(false);
    expect(shouldLabelAtZoom(0.5, -Infinity)).toBe(false);
  });
});
