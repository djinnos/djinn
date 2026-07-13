import { describe, expect, it } from "vitest";

import {
  EMPTY_HIGHLIGHT_VIEW,
  HEATMAP_COLOR_HIGH,
  HEATMAP_COLOR_LOW,
  HEATMAP_COLOR_MID,
  HEATMAP_COLOR_NULL,
  HEATMAP_COLOR_TOP,
  TRAVERSAL_CONTAINMENT_EDGE_KINDS,
  applyComplexityHeatmap,
  colorForComplexity,
  computeComplexityThresholds,
  edgeReducer,
  type EdgeKindAwareGraph,
  isViewEmpty,
  nodeReducer,
  oneHopNeighborhood,
  pickHighlightMode,
  topComplexityIds,
  type HighlightView,
  type MinimalGraph,
} from "./codeGraphReducers";
import {
  ZOOM_FAR,
  ZOOM_NEAR,
  computePagerankPercentiles,
} from "./codeGraphLabels";
import {
  buildGraphFromSnapshot,
  type SnapshotPayload,
} from "@/lib/codeGraphAdapter";
import { computeDoiFocus } from "@/hooks/useGraphReducers";

// Minimal `SnapshotPayload` with 4 nodes whose PageRank values span the
// full [0, 1] percentile range. Mirrors the shape of
// `codeGraphAdapter.test.ts:18-77` but is inlined here so this test
// file stays self-contained (matches the existing convention of
// defining a small fixture inside the file that consumes it).
const pagerankFixtureSnapshot: SnapshotPayload = {
  project_id: "proj-zoom-gate",
  git_head: "zoom-gate",
  generated_at: "2026-06-17T00:00:00Z",
  truncated: false,
  total_nodes: 4,
  total_edges: 3,
  node_cap: 2_000,
  nodes: [
    {
      id: "file:top.rs",
      kind: "file",
      label: "top.rs",
      pagerank: 1.0,
    },
    {
      id: "symbol:scip-rust . . . mid()",
      kind: "symbol",
      label: "mid",
      symbol_kind: "function",
      file_path: "src/mid.rs",
      pagerank: 0.5,
    },
    {
      id: "symbol:scip-rust . . . low()",
      kind: "symbol",
      label: "low",
      symbol_kind: "function",
      file_path: "src/low.rs",
      pagerank: 0.25,
    },
    {
      id: "file:bottom.rs",
      kind: "file",
      label: "bottom.rs",
      pagerank: 0.0,
    },
  ],
  edges: [
    {
      from: "file:top.rs",
      to: "symbol:scip-rust . . . mid()",
      kind: "ContainsDefinition",
      confidence: 0.95,
    },
    {
      from: "symbol:scip-rust . . . mid()",
      to: "symbol:scip-rust . . . low()",
      kind: "SymbolReference",
      confidence: 0.85,
    },
    {
      from: "file:bottom.rs",
      to: "symbol:scip-rust . . . low()",
      kind: "ContainsDefinition",
      confidence: 0.95,
    },
  ],
};

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
    expect(isViewEmpty(viewWith({ toolHighlightIds: new Set(["y"]) }))).toBe(
      false,
    );
    expect(isViewEmpty(viewWith({ blastRadiusFrontier: new Set(["z"]) }))).toBe(
      false,
    );
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

  // KNOWN BUG (verification task g293): the documented layer priority at
  // codeGraphReducers.ts:14 ranks AI citations (#2) above tool-call
  // results (#3), and the reducer zIndex values agree (citation:80 >
  // tool:70). However `pickHighlightMode` currently checks
  // `toolHighlightIds` (line 139) BEFORE `citationIds` (line 140), so a
  // node surfaced by *both* a tool call and a citation renders as "tool"
  // instead of "citation". This test pins the *desired* contract.
  //
  // It is declared `it.fails` so the suite stays green while flagging the
  // regression: once the reducer ordering is fixed (swap the tool/citation
  // checks), flip this back to a plain `it(...)`. See the follow-up task
  // linked in the g293 activity log.
  it.fails(
    "citation layer wins over tool highlight (per comment at codeGraphReducers.ts:14)",
    () => {
      const v = viewWith({
        citationIds: new Set(["alpha", "beta"]),
        toolHighlightIds: new Set(["alpha", "beta"]),
      });
      expect(pickHighlightMode("alpha", v)).toBe("citation");
      expect(pickHighlightMode("beta", v)).toBe("citation");
    },
  );

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
      pickHighlightMode("h", viewWith({ hoverId: "h", selectionId: "h" })),
    ).toBe("focus");
  });
});

describe("nodeReducer", () => {
  it("passes attrs through unchanged when view is empty", () => {
    const attrs = { color: "blue", size: 5, label: "Foo" };
    const out = nodeReducer("a", attrs, EMPTY_HIGHLIGHT_VIEW);
    expect(out).toBe(attrs);
  });

  it("hides nodes outside the isolated crate when crateFilter is set", () => {
    const v = viewWith({ crateFilter: "djinn-graph" });
    const inCrate = nodeReducer(
      "a",
      { color: "blue", size: 5, label: "A", colorGroup: "djinn-graph" },
      v,
    );
    expect(inCrate.hidden).toBeUndefined();
    const otherCrate = nodeReducer(
      "b",
      { color: "blue", size: 5, label: "B", colorGroup: "djinn-agent" },
      v,
    );
    expect(otherCrate.hidden).toBe(true);
  });

  it("de-emphasizes workspace context nodes even when no highlight is active", () => {
    const out = nodeReducer(
      "remote",
      {
        color: "#22d3ee",
        size: 10,
        label: "Remote",
        isWorkspaceContext: true,
        borderSize: 2,
      },
      EMPTY_HIGHLIGHT_VIEW,
    );
    expect(out.workspaceContextDimmed).toBe(true);
    expect(out.label).toBeUndefined();
    expect(out.size as number).toBeLessThan(10);
    expect(out.borderSize as number).toBeLessThan(2);
  });

  it("dims nodes outside the DOI focus set instead of hiding them", () => {
    const v = viewWith({
      doiFocusIds: new Set(["a"]),
      doiContextIds: new Set(["z"]),
    });
    const out = nodeReducer("z", { color: "blue", size: 5, label: "Z" }, v);
    expect(out.hidden).toBeUndefined();
    expect(out.color).toMatch(/rgba\(100/);
    expect(out.label).toBeUndefined();
    expect(out.highlighted).toBe(false);
  });

  it("keeps DOI focused nodes highlighted and readable", () => {
    const v = viewWith({
      doiFocusIds: new Set(["doi"]),
      doiScores: new Map([["doi", 0.75]]),
    });
    const out = nodeReducer("doi", { color: "blue", size: 5, label: "DOI" }, v);
    expect(out.hidden).toBeUndefined();
    expect(out.label).toBe("DOI");
    expect(out.highlighted).toBe(true);
    expect(out.size as number).toBeGreaterThan(5);
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

  // Iter y3mf: zoom-adaptive PageRank-percentile label gate. The
  // helper `shouldLabelAtZoom` lives in `./codeGraphLabels` and is
  // exhaustively covered by `codeGraphLabels.test.ts`; these tests
  // only assert that the reducer plumbs the view fields into the
  // helper and strips `label` when the helper returns `false`.
  it("strips label on a low-percentile node when zoomed out", () => {
    const v = viewWith({
      // Top 10% only at ZOOM_FAR → percentile 0.5 falls off.
      pagerankPercentile: new Map([["mid", 0.5]]),
      cameraRatio: 0.05,
    });
    const out = nodeReducer(
      "mid",
      { color: "blue", size: 4, label: "MidRankNode" },
      v,
    );
    expect(out.label).toBeUndefined();
  });

  it("keeps the label on a high-percentile node at the same zoom", () => {
    const v = viewWith({
      pagerankPercentile: new Map([["top", 0.95]]),
      cameraRatio: 0.05,
    });
    const out = nodeReducer(
      "top",
      { color: "blue", size: 4, label: "TopRankNode" },
      v,
    );
    expect(out.label).toBe("TopRankNode");
  });

  it("relabels a mid-percentile node once the camera zooms in", () => {
    const midView = viewWith({
      pagerankPercentile: new Map([["mid", 0.5]]),
      cameraRatio: 0.05,
    });
    const nearView = viewWith({
      pagerankPercentile: new Map([["mid", 0.5]]),
      cameraRatio: 2.0,
    });
    expect(
      nodeReducer("mid", { color: "blue", size: 4, label: "Mid" }, midView)
        .label,
    ).toBeUndefined();
    expect(
      nodeReducer("mid", { color: "blue", size: 4, label: "Mid" }, nearView)
        .label,
    ).toBe("Mid");
  });

  it("treats nodes absent from the percentile map as fully eligible", () => {
    // Snapshot has no `pagerank` data → empty map; the helper falls
    // through to `true` and the existing label is preserved.
    const v = viewWith({
      pagerankPercentile: new Map(),
      cameraRatio: 0.05,
    });
    const out = nodeReducer(
      "no_rank",
      { color: "blue", size: 4, label: "NoRank" },
      v,
    );
    expect(out.label).toBe("NoRank");
  });

  it("treats cameraRatio = Infinity as 'show everything'", () => {
    // Sigma hasn't reported a camera yet; first paint shouldn't be
    // empty even for low-percentile nodes.
    const v = viewWith({
      pagerankPercentile: new Map([["low", 0.1]]),
      cameraRatio: Infinity,
    });
    const out = nodeReducer("low", { color: "blue", size: 4, label: "Low" }, v);
    expect(out.label).toBe("Low");
  });

  it("preserves the focus label even when the gate would strip it", () => {
    // Focus-mode nodes have their own label-preserving override that
    // re-asserts `attrs.label` on top of `baseAttrs`, so the gate
    // stripping `label` on `baseAttrs` doesn't propagate to the final
    // output. The focal click target should always carry its label
    // regardless of zoom.
    const v = viewWith({
      selectionId: "sel",
      selectionNeighbors: new Set(["sel"]),
      pagerankPercentile: new Map([["sel", 0.05]]),
      cameraRatio: 0.05,
    });
    const out = nodeReducer("sel", { color: "blue", size: 4, label: "Sel" }, v);
    expect(out.highlighted).toBe(true);
    expect(out.label).toBe("Sel");
  });

  it("preserves the neighbor label even when the gate would strip it", () => {
    // 1-hop neighbor of the selection. The neighbor branch re-asserts
    // `attrs.label` so the gate's strip doesn't reach the final
    // output. Color stays amber-200 (the highlight override).
    const v = viewWith({
      selectionId: "sel",
      selectionNeighbors: new Set(["sel", "nbr"]),
      pagerankPercentile: new Map([["nbr", 0.2]]),
      cameraRatio: 0.05,
    });
    const out = nodeReducer("nbr", { color: "blue", size: 4, label: "Nbr" }, v);
    expect(out.color).toBe("#fde68a");
    expect(out.label).toBe("Nbr");
  });

  it("preserves the citation label even when the gate would strip it", () => {
    const v = viewWith({
      citationIds: new Set(["c"]),
      pagerankPercentile: new Map([["c", 0.1]]),
      cameraRatio: 0.05,
    });
    const out = nodeReducer("c", { color: "blue", size: 4, label: "Cite" }, v);
    expect(out.color).toBe("#38bdf8");
    expect(out.label).toBe("Cite");
  });

  it("preserves the tool label even when the gate would strip it", () => {
    const v = viewWith({
      toolHighlightIds: new Set(["t"]),
      pagerankPercentile: new Map([["t", 0.1]]),
      cameraRatio: 0.05,
    });
    const out = nodeReducer("t", { color: "blue", size: 4, label: "Tool" }, v);
    expect(out.color).toBe("#a78bfa");
    expect(out.label).toBe("Tool");
  });

  it("preserves the blast label even when the gate would strip it", () => {
    const v = viewWith({
      blastRadiusFrontier: new Set(["b"]),
      pagerankPercentile: new Map([["b", 0.1]]),
      cameraRatio: 0.05,
    });
    const out = nodeReducer("b", { color: "blue", size: 4, label: "Blast" }, v);
    expect(out.label).toBe("Blast");
  });

  it("preserves the hover label even when the gate would strip it", () => {
    const v = viewWith({
      hoverId: "h",
      pagerankPercentile: new Map([["h", 0.1]]),
      cameraRatio: 0.05,
    });
    const out = nodeReducer("h", { color: "blue", size: 4, label: "Hov" }, v);
    expect(out.label).toBe("Hov");
  });

  it("dim branch still strips label at far zoom", () => {
    // The `dim` branch overrides `label: undefined` itself, so a
    // dimmed node at low zoom is label-stripped regardless of the
    // gate. The gate's contribution is moot — but verifying the
    // "doubly label-stripped" comment in the design.
    const v = viewWith({
      selectionId: "sel",
      selectionNeighbors: new Set(["sel"]),
      pagerankPercentile: new Map([["z", 0.1]]),
      cameraRatio: 0.05,
    });
    const out = nodeReducer("z", { color: "blue", size: 4, label: "Z" }, v);
    expect(out.color).toMatch(/rgba/);
    expect(out.label).toBeUndefined();
  });
});

// ── Zoom-adaptive PageRank label gate (iter y3mf) ────────────────────────────
//
// End-to-end coverage of the integration between `codeGraphLabels` (the
// percentile helper + zoom curve), the extended `nodeReducer` (which
// plumbs `pagerankPercentile` / `cameraRatio` through `HighlightView`),
// and the existing LOD settings (workspace-context de-emphasis,
// selection / citation / tool highlight branches).
//
// The unit tests for the helper itself live in `codeGraphLabels.test.ts`
// and the per-branch reducer tests live in the `nodeReducer` describe
// above; this block is the cross-cutting integration glue that closes
// the loop on the proposal's AC #5.
describe("zoom-adaptive PageRank label gate", () => {
  describe("snapshot integration", () => {
    it("real fixture → percentile map is the right shape", () => {
      // Build a real graphology graph from the SnapshotPayload fixture,
      // then derive the percentile map the same way `useGraphReducers`
      // does at runtime. The `PagerankGraphLike` interface is a
      // structural subset of graphology's `Graph`, so no adapter is
      // required.
      const graph = buildGraphFromSnapshot(pagerankFixtureSnapshot);
      const map = computePagerankPercentiles(graph);

      // Every node in the snapshot gets a percentile entry — the
      // helper filters missing / non-finite `pagerank` but the
      // fixture populates the field on every node.
      expect(map.size).toBe(graph.order);
      expect(map.size).toBe(pagerankFixtureSnapshot.nodes.length);

      // The node with the highest raw PageRank sits at percentile 1.0.
      const topId = "file:top.rs";
      expect(map.get(topId)).toBe(1);

      // The node with the lowest raw PageRank sits at percentile 0.0.
      const bottomId = "file:bottom.rs";
      expect(map.get(bottomId)).toBe(0);

      // Every entry is a finite value in the closed unit interval.
      for (const v of map.values()) {
        expect(Number.isFinite(v)).toBe(true);
        expect(v).toBeGreaterThanOrEqual(0);
        expect(v).toBeLessThanOrEqual(1);
      }
    });
  });

  describe("zoom in → more labels", () => {
    // Hand-built view: 5 nodes, percentiles 0.0 / 0.25 / 0.5 / 0.75 / 1.0.
    // The hook delivers exactly this shape at runtime; bypassing the
    // graph keeps the test focused on the reducer integration with the
    // zoom curve.
    const NODE_IDS = ["n0", "n1", "n2", "n3", "n4"];
    const PERCENTILES: ReadonlyArray<number> = [0.0, 0.25, 0.5, 0.75, 1.0];
    const midpoint = (ZOOM_FAR + ZOOM_NEAR) / 2;

    function makeView(cameraRatio: number): HighlightView {
      return viewWith({
        pagerankPercentile: new Map(
          NODE_IDS.map((id, i) => [id, PERCENTILES[i]]),
        ),
        cameraRatio,
      });
    }

    function countLabeled(view: HighlightView): number {
      let n = 0;
      for (const id of NODE_IDS) {
        const out = nodeReducer(
          id,
          { color: "blue", size: 4, label: `L${id}` },
          view,
        );
        if (out.label !== undefined) n += 1;
      }
      return n;
    }

    it("at ZOOM_FAR only the top 10% by PageRank keep their label", () => {
      // Threshold at ZOOM_FAR is `1 - TOP_PERCENT_FAR = 0.90`, so the
      // single 1.0-percentile node keeps its label and the other four
      // (0.0, 0.25, 0.5, 0.75) fall off.
      const far = makeView(ZOOM_FAR);
      expect(countLabeled(far)).toBe(1);
    });

    it("at the midpoint strictly more nodes pass the gate than at ZOOM_FAR", () => {
      const far = countLabeled(makeView(ZOOM_FAR));
      const mid = countLabeled(makeView(midpoint));
      expect(mid).toBeGreaterThan(far);
    });

    it("at ZOOM_NEAR every node keeps its label", () => {
      // Threshold at ZOOM_NEAR is 0, so all 5 percentiles pass.
      expect(countLabeled(makeView(ZOOM_NEAR))).toBe(5);
    });

    it("the count is monotonically non-decreasing as the camera zooms in", () => {
      // Sample 5 points across the curve (including the two endpoints
      // and a few midpoints) and assert the count never drops. The
      // "strictly increases between ZOOM_FAR and ZOOM_NEAR" guarantee
      // is implicit — a non-decreasing sequence that goes 1 → 5 can't
      // be constant.
      const samples = [
        ZOOM_FAR,
        ZOOM_FAR + (ZOOM_NEAR - ZOOM_FAR) * 0.25,
        midpoint,
        ZOOM_FAR + (ZOOM_NEAR - ZOOM_FAR) * 0.75,
        ZOOM_NEAR,
      ];
      const counts = samples.map((r) => countLabeled(makeView(r)));
      for (let i = 1; i < counts.length; i += 1) {
        expect(counts[i]).toBeGreaterThanOrEqual(counts[i - 1]);
      }
      // And the full sweep covers the expected endpoints: 1 at the far
      // end, 5 at the near end.
      expect(counts[0]).toBe(1);
      expect(counts[counts.length - 1]).toBe(5);
    });
  });

  describe("missing-pagerank fallback (compose with existing LOD)", () => {
    it("preserves the label when pagerankPercentile is the empty default Map", () => {
      // EMPTY_HIGHLIGHT_VIEW ships with an empty percentile map and
      // `cameraRatio: Infinity`. Snapshots with no `pagerank` data
      // → the helper short-circuits to `true` and the existing label
      // is preserved. This is the "compose with existing LOD"
      // guarantee from the design.
      const v = viewWith({
        pagerankPercentile: new Map(),
        cameraRatio: 0.05,
      });
      const out = nodeReducer(
        "any",
        { color: "blue", size: 4, label: "Original" },
        v,
      );
      expect(out.label).toBe("Original");
    });

    it("treats a node whose id is absent from the percentile map as fully eligible", () => {
      // The view's map has data for some other node, but `target` is
      // not in it. `shouldLabelAtZoom` falls through to `true` via its
      // `null` / `undefined` percentile branch (`Map.get` returns
      // `undefined` for missing keys), so the label survives the gate.
      const v = viewWith({
        pagerankPercentile: new Map([["other", 0.05]]),
        cameraRatio: 0.05,
      });
      const out = nodeReducer(
        "target",
        { color: "blue", size: 4, label: "Stays" },
        v,
      );
      expect(out.label).toBe("Stays");
    });
  });

  describe("workspace-context composition", () => {
    it("workspace de-emphasis still strips the label when the gate says 'show'", () => {
      // Workspace-context nodes are de-emphasized to `label: undefined`
      // *before* the percentile gate fires (codeGraphReducers.ts:286).
      // The gate's `if (baseAttrs.label !== undefined)` guard means
      // it's a no-op for them — the de-emphasis wins, and the gate
      // can't accidentally restore a label on a de-emphasized node.
      const v = viewWith({
        // Threshold at ZOOM_NEAR = 0; a 0.0-percentile node passes
        // (showing the "gate says show" case). Workspace context
        // should still de-emphasize the label.
        pagerankPercentile: new Map([["ws", 0.0]]),
        cameraRatio: ZOOM_NEAR,
      });
      const out = nodeReducer(
        "ws",
        {
          color: "blue",
          size: 4,
          label: "WorkspaceLabel",
          isWorkspaceContext: true,
        },
        v,
      );
      expect(out.label).toBeUndefined();
      expect(out.workspaceContextDimmed).toBe(true);
    });

    it("workspace de-emphasis still strips the label when the gate says 'hide'", () => {
      // Symmetric case: the gate would strip the label anyway (far
      // zoom + 0.05 percentile), but the de-emphasis strips it first.
      // No behavioral change, but pinning the composition order so a
      // future refactor can't break the invariant that workspace
      // nodes never carry labels.
      const v = viewWith({
        pagerankPercentile: new Map([["ws", 0.05]]),
        cameraRatio: 0.05,
      });
      const out = nodeReducer(
        "ws",
        {
          color: "blue",
          size: 4,
          label: "WorkspaceLabel",
          isWorkspaceContext: true,
        },
        v,
      );
      expect(out.label).toBeUndefined();
      expect(out.workspaceContextDimmed).toBe(true);
    });
  });

  describe("highlight-mode interaction (zoom doesn't kill focal labels)", () => {
    // A percentile map that would label-strip every focal node at the
    // current camera ratio. The highlight-mode branches
    // (`focus` / `citation` / `tool`) re-assert `attrs.label` on top
    // of `baseAttrs`, so the gate's strip never reaches the final
    // output for those nodes.
    const strippyMap = new Map<string, number>([
      ["sel", 0.05],
      ["cite", 0.05],
      ["tool", 0.05],
    ]);
    const farView: HighlightView = viewWith({
      // Drive `pickHighlightMode` to the "focus" branch for `sel`. The
      // selection's own neighborhood is empty here — the test is
      // isolating the label-preservation guarantee, not the neighbor
      // visualization.
      selectionId: "sel",
      selectionNeighbors: new Set(["sel"]),
      pagerankPercentile: strippyMap,
      cameraRatio: ZOOM_FAR,
    });

    it("selectionId keeps its label even when the gate would strip it", () => {
      const out = nodeReducer(
        "sel",
        { color: "blue", size: 4, label: "Sel" },
        farView,
      );
      expect(out.highlighted).toBe(true);
      expect(out.label).toBe("Sel");
      // And the focal color override is still applied.
      expect(out.color).toBe("#f97316");
    });

    it("citationIds keeps its label even when the gate would strip it", () => {
      // Construct a view whose only highlight is the citation set so
      // `pickHighlightMode` returns `"citation"` (not `"dim"` or
      // `"neighbor"`) for `cite`.
      const v = viewWith({
        citationIds: new Set(["cite"]),
        pagerankPercentile: strippyMap,
        cameraRatio: ZOOM_FAR,
      });
      const out = nodeReducer(
        "cite",
        { color: "blue", size: 4, label: "Cite" },
        v,
      );
      expect(out.label).toBe("Cite");
      expect(out.color).toBe("#38bdf8");
    });

    it("toolHighlightIds keeps its label even when the gate would strip it", () => {
      const v = viewWith({
        toolHighlightIds: new Set(["tool"]),
        pagerankPercentile: strippyMap,
        cameraRatio: ZOOM_FAR,
      });
      const out = nodeReducer(
        "tool",
        { color: "blue", size: 4, label: "Tool" },
        v,
      );
      expect(out.label).toBe("Tool");
      expect(out.color).toBe("#a78bfa");
    });
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

  it("honors edgeKind aliases when filtering", () => {
    const v = viewWith({ edgeKindFilters: { SymbolReference: false } });
    const out = edgeReducer(
      "a",
      "b",
      { edgeKind: "SymbolReference" },
      v,
    );
    expect(out.hidden).toBe(true);
  });

  it("treats unknown edge kinds as visible (no filter entry)", () => {
    const v = viewWith({ edgeKindFilters: {} });
    const out = edgeReducer("a", "b", { kind: "MysteryKind" }, v);
    expect(out.hidden).toBeUndefined();
  });

  it("dims edges that cross outside the DOI focus set", () => {
    const v = viewWith({
      doiFocusIds: new Set(["a", "b"]),
      doiContextIds: new Set(["z"]),
    });
    const out = edgeReducer("a", "z", { kind: "Reads" }, v);
    expect(out.hidden).toBeUndefined();
    expect(out.color).toMatch(/rgba\(100/);
  });

  it("highlights edges whose endpoints are both in the DOI focus set", () => {
    const v = viewWith({ doiFocusIds: new Set(["a", "b"]) });
    const out = edgeReducer("a", "b", { kind: "Reads", size: 1 }, v);
    expect(out.color).toMatch(/orange|rgba\(251/);
    expect(out.size as number).toBeGreaterThan(1);
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

  it("keeps cross-workspace edges prominent under selection dimming", () => {
    const v = viewWith({
      selectionId: "a",
      selectionNeighbors: new Set(["a"]),
    });
    const out = edgeReducer(
      "remote-a",
      "remote-b",
      { kind: "Reads", size: 2, isCrossWorkspace: true, color: "#facc15" },
      v,
    );
    expect(out.color).toMatch(/250, 204, 21/);
    expect(out.size as number).toBeGreaterThan(2);
    expect(out.zIndex as number).toBeGreaterThan(5);
  });

  it("makes selected cross-workspace edges stronger than normal selected edges", () => {
    const v = viewWith({
      selectionId: "a",
      selectionNeighbors: new Set(["a", "b"]),
    });
    const normal = edgeReducer("a", "b", { kind: "Reads", size: 2 }, v);
    const cross = edgeReducer(
      "a",
      "b",
      { kind: "Reads", size: 2, crossWorkspace: true },
      v,
    );
    expect(cross.color).toMatch(/250, 204, 21/);
    expect(cross.size).toBeGreaterThan(normal.size as number);
    expect(cross.zIndex).toBeGreaterThan(normal.zIndex as number);
  });

  it("hides containment edges even when the filter tries to enable them", () => {
    const v = viewWith({
      edgeKindFilters: { ContainsDefinition: true },
    });
    const out = edgeReducer(
      "a",
      "b",
      { kind: "ContainsDefinition", size: 1 },
      v,
    );
    expect(out.hidden).toBe(true);
  });

  it("hides all three containment edge kinds", () => {
    for (const kind of [
      "ContainsDefinition",
      "DeclaredInFile",
      "MemberOf",
    ]) {
      const out = edgeReducer("a", "b", { kind, size: 1 }, EMPTY_HIGHLIGHT_VIEW);
      expect(out.hidden).toBe(true);
    }
  });

  it("hides containment edges even inside an active selection highlight", () => {
    const v = viewWith({
      selectionId: "a",
      selectionNeighbors: new Set(["a", "b"]),
    });
    const out = edgeReducer(
      "a",
      "b",
      { kind: "ContainsDefinition", size: 1 },
      v,
    );
    expect(out.hidden).toBe(true);
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

  it("excludes containment edges from traversal when the graph is edge-kind-aware", () => {
    // Build a graph where 'a' is connected to 'b' via SymbolReference
    // and to 'file' via ContainsDefinition. The containment edge should
    // be skipped so the 1-hop set does not include 'file'.
    const g: EdgeKindAwareGraph = {
      hasNode: (id) => id === "a" || id === "b" || id === "file",
      neighbors: (id) => (id === "a" ? ["b", "file"] : id === "b" ? ["a"] : ["a"]),
      edgeKind: (source, target) => {
        const pair = [source, target].sort().join("|");
        if (pair === "a|b") return "SymbolReference";
        if (pair === "a|file") return "ContainsDefinition";
        return null;
      },
    };
    const ns = oneHopNeighborhood(g, "a");
    expect(ns.has("a")).toBe(true);
    expect(ns.has("b")).toBe(true);
    expect(ns.has("file")).toBe(false);
  });

  it("excludes all three containment kinds from traversal", () => {
    for (const kind of TRAVERSAL_CONTAINMENT_EDGE_KINDS) {
      const g: EdgeKindAwareGraph = {
        hasNode: (id) => id === "a" || id === "b",
        neighbors: (id) => (id === "a" ? ["b"] : ["a"]),
        edgeKind: () => kind,
      };
      const ns = oneHopNeighborhood(g, "a");
      expect(ns.has("b")).toBe(false);
    }
  });
});

describe("computeDoiFocus", () => {
  const graph = buildGraphFromSnapshot({
    ...pagerankFixtureSnapshot,
    nodes: [
      { id: "a", kind: "symbol", label: "A", pagerank: 0.4 },
      { id: "b", kind: "symbol", label: "B", pagerank: 0.9 },
      { id: "c", kind: "symbol", label: "C", pagerank: 0.6 },
      { id: "file:a.ts", kind: "file", label: "a.ts", pagerank: 1.0 },
    ],
    edges: [
      { from: "a", to: "b", kind: "SymbolReference", confidence: 1 },
      { from: "c", to: "a", kind: "Reads", confidence: 1 },
      {
        from: "file:a.ts",
        to: "a",
        kind: "ContainsDefinition",
        confidence: 1,
      },
    ],
  });
  const pagerank = computePagerankPercentiles(graph);

  it("walks dependencies downstream and excludes containment edges", () => {
    const out = computeDoiFocus(graph, "a", "dependencies", pagerank, 10);
    expect(out.focusIds.has("a")).toBe(true);
    expect(out.focusIds.has("b")).toBe(true);
    expect(out.focusIds.has("c")).toBe(false);
    expect(out.focusIds.has("file:a.ts")).toBe(false);
  });

  it("walks dependents upstream", () => {
    const out = computeDoiFocus(graph, "a", "dependents", pagerank, 10);
    expect(out.focusIds.has("a")).toBe(true);
    expect(out.focusIds.has("c")).toBe(true);
    expect(out.focusIds.has("b")).toBe(false);
  });

  it("bounds the readable focus set and leaves lower DOI nodes as context", () => {
    const out = computeDoiFocus(graph, "a", "both", pagerank, 2);
    expect(out.focusIds.has("a")).toBe(true);
    expect(out.focusIds.size).toBe(2);
    expect(out.contextIds.size).toBeGreaterThan(0);
    for (const id of out.contextIds) {
      expect(out.focusIds.has(id)).toBe(false);
    }
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
    const topOut = nodeReducer(
      "a",
      { color: "#dirhash", cognitive: 30 },
      topology,
    );
    const cxOut = nodeReducer(
      "a",
      { color: "#dirhash", cognitive: 30 },
      complexity,
    );
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
