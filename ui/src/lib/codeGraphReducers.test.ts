import { describe, expect, it } from "vitest";

import {
  EMPTY_HIGHLIGHT_VIEW,
  HEATMAP_COLOR_HIGH,
  HEATMAP_COLOR_LOW,
  HEATMAP_COLOR_MID,
  HEATMAP_COLOR_NULL,
  HEATMAP_COLOR_TOP,
  applyComplexityHeatmap,
  bfsReachable,
  colorForComplexity,
  computeComplexityThresholds,
  edgeReducer,
  isViewEmpty,
  nodeReducer,
  oneHopNeighborhood,
  pickHighlightMode,
  topComplexityIds,
  type HighlightView,
  type MinimalGraph,
} from "./codeGraphReducers";

function viewWith(overrides: Partial<HighlightView>): HighlightView {
  return { ...EMPTY_HIGHLIGHT_VIEW, ...overrides };
}

/** Tiny adjacency-list graph for the BFS / 1-hop tests. */
function makeGraph(
  edges: Array<[string, string]>,
  nodes?: string[],
): MinimalGraph {
  const adj = new Map<string, Set<string>>();
  const ensureNode = (id: string) => {
    if (!adj.has(id)) adj.set(id, new Set());
  };
  for (const id of nodes ?? []) ensureNode(id);
  for (const [a, b] of edges) {
    ensureNode(a);
    ensureNode(b);
    adj.get(a)!.add(b);
    adj.get(b)!.add(a); // undirected for the highlight reducer
  }
  return {
    hasNode: (id) => adj.has(id),
    neighbors: (id) => Array.from(adj.get(id) ?? []),
  };
}

describe("isViewEmpty", () => {
  it("is true for the default view", () => {
    expect(isViewEmpty(EMPTY_HIGHLIGHT_VIEW)).toBe(true);
  });

  it("is false once selection is set", () => {
    expect(isViewEmpty(viewWith({ selectionId: "a" }))).toBe(false);
  });

  it("is false when any highlight set is non-empty", () => {
    expect(isViewEmpty(viewWith({ citationIds: new Set(["x"]) }))).toBe(false);
    expect(
      isViewEmpty(viewWith({ toolHighlightIds: new Set(["y"]) })),
    ).toBe(false);
    expect(
      isViewEmpty(viewWith({ blastRadiusFrontier: new Set(["z"]) })),
    ).toBe(false);
  });

  it("is false when hover is set", () => {
    expect(isViewEmpty(viewWith({ hoverId: "h" }))).toBe(false);
  });
});

describe("pickHighlightMode", () => {
  it("returns 'none' on the empty view", () => {
    expect(pickHighlightMode("a", EMPTY_HIGHLIGHT_VIEW)).toBe("none");
  });

  it("focuses the selection node", () => {
    const v = viewWith({ selectionId: "a" });
    expect(pickHighlightMode("a", v)).toBe("focus");
  });

  it("highlights neighbors of the selection", () => {
    const v = viewWith({
      selectionId: "a",
      selectionNeighbors: new Set(["a", "b"]),
    });
    expect(pickHighlightMode("b", v)).toBe("neighbor");
  });

  it("dims unrelated nodes when a selection exists", () => {
    const v = viewWith({
      selectionId: "a",
      selectionNeighbors: new Set(["a"]),
    });
    expect(pickHighlightMode("z", v)).toBe("dim");
  });

  it("blast radius beats tool highlight", () => {
    const v = viewWith({
      blastRadiusFrontier: new Set(["a"]),
      toolHighlightIds: new Set(["a"]),
    });
    expect(pickHighlightMode("a", v)).toBe("blast");
  });

  it("tool highlight beats citation", () => {
    const v = viewWith({
      toolHighlightIds: new Set(["a"]),
      citationIds: new Set(["a"]),
    });
    expect(pickHighlightMode("a", v)).toBe("tool");
  });

  it("citation beats neighbor", () => {
    const v = viewWith({
      citationIds: new Set(["b"]),
      selectionId: "a",
      selectionNeighbors: new Set(["a", "b"]),
    });
    expect(pickHighlightMode("b", v)).toBe("citation");
  });

  it("hover surfaces only when nothing else applies", () => {
    const v = viewWith({ hoverId: "h" });
    expect(pickHighlightMode("h", v)).toBe("hover");
    // Selection wins over hover
    expect(
      pickHighlightMode(
        "h",
        viewWith({ hoverId: "h", selectionId: "h" }),
      ),
    ).toBe("focus");
  });
});

describe("nodeReducer", () => {
  it("passes attrs through unchanged when view is empty", () => {
    const attrs = { color: "blue", size: 5, label: "Foo" };
    const out = nodeReducer("a", attrs, EMPTY_HIGHLIGHT_VIEW);
    expect(out).toBe(attrs);
  });

  it("hides nodes outside the depth frontier", () => {
    const v = viewWith({
      selectionId: "a",
      depthReachable: new Set(["a"]),
    });
    const out = nodeReducer("z", { color: "blue", size: 5 }, v);
    expect(out.hidden).toBe(true);
  });

  it("paints the focal node orange and grows it", () => {
    const v = viewWith({
      selectionId: "a",
      selectionNeighbors: new Set(["a"]),
    });
    const out = nodeReducer("a", { color: "blue", size: 4 }, v);
    expect(out.color).toBe("#f97316");
    expect(out.size).toBe(4 * 1.6);
    expect(out.highlighted).toBe(true);
  });

  it("dims a non-neighbor when selection is active", () => {
    const v = viewWith({
      selectionId: "a",
      selectionNeighbors: new Set(["a"]),
    });
    const out = nodeReducer("z", { color: "blue", size: 4, label: "Z" }, v);
    expect(out.color).toMatch(/rgba/);
    expect(out.label).toBeUndefined();
    expect(out.highlighted).toBe(false);
  });

  it("renders citation nodes in sky-blue", () => {
    const v = viewWith({ citationIds: new Set(["c"]) });
    const out = nodeReducer("c", { color: "blue", size: 4 }, v);
    expect(out.color).toBe("#38bdf8");
  });

  it("renders tool-highlight nodes in violet", () => {
    const v = viewWith({ toolHighlightIds: new Set(["t"]) });
    const out = nodeReducer("t", { color: "blue", size: 4 }, v);
    expect(out.color).toBe("#a78bfa");
  });

  it("blast-radius pulse interpolates color across the phase cycle", () => {
    const lo = viewWith({
      blastRadiusFrontier: new Set(["b"]),
      pulsePhase: 0,
    });
    const hi = viewWith({
      blastRadiusFrontier: new Set(["b"]),
      pulsePhase: 0.5,
    });
    const outLo = nodeReducer("b", { color: "blue", size: 4 }, lo);
    const outHi = nodeReducer("b", { color: "blue", size: 4 }, hi);
    expect(outLo.color).not.toBe(outHi.color);
  });
});

describe("edgeReducer", () => {
  it("passes through when view is empty and no kind filter applies", () => {
    const attrs = { color: "gray", size: 1, kind: "Reads" };
    const out = edgeReducer("a", "b", attrs, EMPTY_HIGHLIGHT_VIEW);
    expect(out).toBe(attrs);
  });

  it("hides edges of disabled kinds", () => {
    const v = viewWith({ edgeKindFilters: { Reads: false } });
    const out = edgeReducer("a", "b", { kind: "Reads" }, v);
    expect(out.hidden).toBe(true);
  });

  it("treats unknown edge kinds as visible (no filter entry)", () => {
    const v = viewWith({ edgeKindFilters: {} });
    const out = edgeReducer("a", "b", { kind: "MysteryKind" }, v);
    expect(out.hidden).toBeUndefined();
  });

  it("hides edges that cross the depth frontier", () => {
    const v = viewWith({
      selectionId: "a",
      depthReachable: new Set(["a", "b"]),
    });
    const out = edgeReducer("a", "z", { kind: "Reads" }, v);
    expect(out.hidden).toBe(true);
  });

  it("highlights edges incident on the selection 1-hop frontier", () => {
    const v = viewWith({
      selectionId: "a",
      selectionNeighbors: new Set(["a", "b"]),
    });
    const out = edgeReducer("a", "b", { kind: "Reads", size: 1 }, v);
    expect(out.color).toMatch(/orange|rgba\(251/);
  });

  it("dims unrelated edges when a selection is active", () => {
    const v = viewWith({
      selectionId: "a",
      selectionNeighbors: new Set(["a"]),
    });
    const out = edgeReducer("y", "z", { kind: "Reads", size: 1 }, v);
    expect(out.color).toMatch(/rgba\(100/);
  });
});

describe("oneHopNeighborhood", () => {
  it("returns empty set for unknown node", () => {
    const g = makeGraph([["a", "b"]]);
    expect(oneHopNeighborhood(g, "missing").size).toBe(0);
  });

  it("includes the seed itself", () => {
    const g = makeGraph([["a", "b"]]);
    const ns = oneHopNeighborhood(g, "a");
    expect(ns.has("a")).toBe(true);
  });

  it("walks undirected neighbors", () => {
    const g = makeGraph([
      ["a", "b"],
      ["c", "a"],
    ]);
    const ns = oneHopNeighborhood(g, "a");
    expect(ns.has("b")).toBe(true);
    expect(ns.has("c")).toBe(true);
  });
});

describe("computeComplexityThresholds", () => {
  it("returns null for an empty sample", () => {
    expect(computeComplexityThresholds([])).toBeNull();
  });

  it("filters out non-finite values and returns null when nothing remains", () => {
    expect(
      computeComplexityThresholds([Number.NaN, Number.POSITIVE_INFINITY]),
    ).toBeNull();
  });

  it("computes ascending p33 / p67 / p90 over a uniform sample", () => {
    // 1..100 — exact-percentile sanity check.
    const values = Array.from({ length: 100 }, (_, i) => i + 1);
    const t = computeComplexityThresholds(values)!;
    expect(t.sampleSize).toBe(100);
    expect(t.p33).toBeLessThan(t.p67);
    expect(t.p67).toBeLessThan(t.p90);
    // numpy default percentile method: clamp(0..1) * (n-1).
    // index = 0.33 * 99 = 32.67 → values[32]=33, values[33]=34 →
    // lerp(33, 34, 0.67) = 33.67
    expect(t.p33).toBeCloseTo(33.67, 1);
    // index = 0.9 * 99 = 89.1 → values[89]=90, values[90]=91 →
    // lerp(90, 91, 0.1) = 90.1
    expect(t.p90).toBeCloseTo(90.1, 1);
  });

  it("handles a single-value sample by returning a flat threshold band", () => {
    const t = computeComplexityThresholds([42])!;
    expect(t.p33).toBe(42);
    expect(t.p67).toBe(42);
    expect(t.p90).toBe(42);
  });
});

describe("colorForComplexity", () => {
  const thresholds = { p33: 5, p67: 10, p90: 20, sampleSize: 100 };

  it("returns the muted-gray bucket for null cognitive", () => {
    expect(colorForComplexity(null, thresholds)).toBe(HEATMAP_COLOR_NULL);
    expect(colorForComplexity(undefined, thresholds)).toBe(HEATMAP_COLOR_NULL);
  });

  it("greens nodes at or below the 33rd percentile", () => {
    expect(colorForComplexity(1, thresholds)).toBe(HEATMAP_COLOR_LOW);
    expect(colorForComplexity(5, thresholds)).toBe(HEATMAP_COLOR_LOW);
  });

  it("yellows nodes between p33 and p67", () => {
    expect(colorForComplexity(6, thresholds)).toBe(HEATMAP_COLOR_MID);
    expect(colorForComplexity(10, thresholds)).toBe(HEATMAP_COLOR_MID);
  });

  it("oranges nodes between p67 and p90", () => {
    expect(colorForComplexity(11, thresholds)).toBe(HEATMAP_COLOR_HIGH);
    expect(colorForComplexity(20, thresholds)).toBe(HEATMAP_COLOR_HIGH);
  });

  it("reds nodes above p90", () => {
    expect(colorForComplexity(21, thresholds)).toBe(HEATMAP_COLOR_TOP);
    expect(colorForComplexity(999, thresholds)).toBe(HEATMAP_COLOR_TOP);
  });
});

describe("topComplexityIds", () => {
  it("returns the top-N ids sorted by cognitive descending", () => {
    const ids = topComplexityIds(
      [
        { id: "a", cognitive: 1 },
        { id: "b", cognitive: 50 },
        { id: "c", cognitive: 10 },
        { id: "d", cognitive: 100 },
        { id: "e", cognitive: null },
      ],
      2,
    );
    expect(ids.has("d")).toBe(true);
    expect(ids.has("b")).toBe(true);
    expect(ids.has("c")).toBe(false);
    expect(ids.has("e")).toBe(false);
    expect(ids.size).toBe(2);
  });

  it("skips null cognitive and returns smaller set when fewer ranked nodes than N", () => {
    const ids = topComplexityIds(
      [
        { id: "a", cognitive: null },
        { id: "b", cognitive: 5 },
      ],
      5,
    );
    expect(ids.size).toBe(1);
    expect(ids.has("b")).toBe(true);
  });

  it("returns an empty set when the input is empty", () => {
    expect(topComplexityIds([], 3).size).toBe(0);
  });
});

describe("applyComplexityHeatmap", () => {
  const thresholds = { p33: 5, p67: 10, p90: 20, sampleSize: 50 };

  it("colors a low-complexity node green", () => {
    const out = applyComplexityHeatmap(
      { color: "#aaaaaa", cognitive: 3 },
      thresholds,
      new Set(),
      "x",
    );
    expect(out.color).toBe(HEATMAP_COLOR_LOW);
    expect(out.haloed).toBeUndefined();
  });

  it("colors a high-complexity node red and adds the halo when in the top-N set", () => {
    const out = applyComplexityHeatmap(
      { color: "#aaaaaa", cognitive: 99 },
      thresholds,
      new Set(["x"]),
      "x",
    );
    expect(out.color).toBe(HEATMAP_COLOR_TOP);
    expect(out.haloed).toBe(true);
    expect(out.borderColor).toBe(HEATMAP_COLOR_TOP);
  });

  it("falls back to gray for a node without cognitive data", () => {
    const out = applyComplexityHeatmap(
      { color: "#aaaaaa" },
      thresholds,
      new Set(),
      "x",
    );
    expect(out.color).toBe(HEATMAP_COLOR_NULL);
  });
});

describe("nodeReducer with complexity heatmap", () => {
  const thresholds = { p33: 5, p67: 10, p90: 20, sampleSize: 50 };

  it("paints the heatmap base color in complexity mode", () => {
    const view: HighlightView = {
      ...EMPTY_HIGHLIGHT_VIEW,
      colorMode: "complexity",
      complexityThresholds: thresholds,
    };
    const lo = nodeReducer("a", { color: "#dirhash", cognitive: 2 }, view);
    const hi = nodeReducer("b", { color: "#dirhash", cognitive: 100 }, view);
    expect(lo.color).toBe(HEATMAP_COLOR_LOW);
    expect(hi.color).toBe(HEATMAP_COLOR_TOP);
  });

  it("preserves topology color in topology mode", () => {
    const view: HighlightView = {
      ...EMPTY_HIGHLIGHT_VIEW,
      colorMode: "topology",
      complexityThresholds: thresholds,
    };
    const out = nodeReducer("a", { color: "#dirhash", cognitive: 100 }, view);
    expect(out.color).toBe("#dirhash");
  });

  it("draws the halo on top-N nodes regardless of color mode", () => {
    const haloIds = new Set(["a"]);
    const topology: HighlightView = {
      ...EMPTY_HIGHLIGHT_VIEW,
      colorMode: "topology",
      complexityHaloIds: haloIds,
    };
    const complexity: HighlightView = {
      ...EMPTY_HIGHLIGHT_VIEW,
      colorMode: "complexity",
      complexityThresholds: thresholds,
      complexityHaloIds: haloIds,
    };
    const topOut = nodeReducer("a", { color: "#dirhash", cognitive: 30 }, topology);
    const cxOut = nodeReducer("a", { color: "#dirhash", cognitive: 30 }, complexity);
    expect(topOut.haloed).toBe(true);
    expect(cxOut.haloed).toBe(true);
    expect(topOut.borderColor).toBe(HEATMAP_COLOR_TOP);
    expect(cxOut.borderColor).toBe(HEATMAP_COLOR_TOP);
  });

  it("selection still wins the color channel over the heatmap base coat", () => {
    const view: HighlightView = {
      ...EMPTY_HIGHLIGHT_VIEW,
      colorMode: "complexity",
      complexityThresholds: thresholds,
      selectionId: "a",
      selectionNeighbors: new Set(["a"]),
    };
    const out = nodeReducer("a", { color: "#dirhash", cognitive: 1 }, view);
    // Focus orange, not heatmap green.
    expect(out.color).toBe("#f97316");
  });

  it("complexity mode is a no-op when thresholds are null", () => {
    const view: HighlightView = {
      ...EMPTY_HIGHLIGHT_VIEW,
      colorMode: "complexity",
      complexityThresholds: null,
    };
    const out = nodeReducer("a", { color: "#dirhash", cognitive: 2 }, view);
    expect(out.color).toBe("#dirhash");
  });
});

describe("bfsReachable", () => {
  const g = makeGraph([
    ["a", "b"],
    ["b", "c"],
    ["c", "d"],
    ["d", "e"],
  ]);

  it("returns just the seed at depth 0", () => {
    const r = bfsReachable(g, "a", 0);
    expect(Array.from(r).sort()).toEqual(["a"]);
  });

  it("walks 1 hop at depth 1", () => {
    const r = bfsReachable(g, "a", 1);
    expect(Array.from(r).sort()).toEqual(["a", "b"]);
  });

  it("walks 3 hops at depth 3", () => {
    const r = bfsReachable(g, "a", 3);
    expect(Array.from(r).sort()).toEqual(["a", "b", "c", "d"]);
  });

  it("walks the entire connected component when depth >> diameter", () => {
    const r = bfsReachable(g, "a", 100);
    expect(Array.from(r).sort()).toEqual(["a", "b", "c", "d", "e"]);
  });

  it("returns empty set for unknown seed", () => {
    expect(bfsReachable(g, "missing", 5).size).toBe(0);
  });
});
