/**
 * CodeGraphCanvas LOD, culling, and progressive expansion tests.
 *
 * The canvas now always fetches at `level="symbol"` and drives visible
 * detail client-side via continuous semantic LOD (camera zoom ratio),
 * viewport culling, and click-to-expand. These tests verify:
 *
 *   - Always issues a single symbol-level fetch (no community fallback).
 *   - LOD tier helpers (`lodTierForZoom`, `isSymbolVisibleAtMidTier`) pin
 *     the tier boundaries.
 *   - Progressive expand (`expandRegion`) is wired to double-click.
 *
 * Sigma / WebGL are mocked out (jsdom doesn't have WebGL) so the canvas
 * renders its overlay without crashing.
 */

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";

import { CodeGraphCanvas } from "./CodeGraphCanvas";
import { useCodeGraphStore } from "@/stores/codeGraphStore";
import type { SnapshotPayload } from "@/lib/codeGraphAdapter";
import {
  lodTierForZoom,
  isSymbolVisibleAtMidTier,
  isInViewport,
  LOD_FAR_RATIO,
  LOD_MID_RATIO,
} from "@/lib/codeGraphAdapter";
import { EMPTY_HIGHLIGHT_VIEW, nodeReducer } from "@/lib/codeGraphReducers";

// ── Mocks ─────────────────────────────────────────────────────────────────

const fetchSnapshotMock = vi.fn();

vi.mock("@/api/codeGraph", () => ({
  fetchSnapshot: (...args: unknown[]) => fetchSnapshotMock(...args),
}));

// The real `useSigmaGraph` needs WebGL; jsdom doesn't have it. Return a
// inert handle so the canvas renders its overlay without crashing.
vi.mock("@/hooks/useSigmaGraph", () => ({
  useSigmaGraph: () => ({
    ready: false,
    layoutRunning: false,
    stopLayout: () => {},
    sigma: null,
  }),
}));

vi.mock("@/hooks/useGraphReducers", () => ({
  useGraphReducers: () => ({
    reducers: {},
    complexityThresholds: null,
    complexityHaloIds: new Set<string>(),
  }),
}));

// RendererCapabilityDialog probes WebGL2 at module load; it returns null
// when supported, so mocking to a no-op avoids the probe in jsdom.
vi.mock("./RendererCapabilityDialog", () => ({
  RendererCapabilityDialog: () => null,
}));

// ── Fixtures ──────────────────────────────────────────────────────────────

function makeSnapshot(
  overrides: Partial<SnapshotPayload> = {},
): SnapshotPayload {
  return {
    project_id: "proj",
    git_head: "abc123",
    generated_at: "2025-01-01T00:00:00Z",
    truncated: false,
    total_nodes: 2,
    total_edges: 1,
    node_cap: 10_000,
    nodes: [
      {
        id: "node-a",
        kind: "symbol",
        label: "alpha",
        symbol_kind: "function",
        pagerank: 0.5,
      },
      {
        id: "node-b",
        kind: "symbol",
        label: "beta",
        symbol_kind: "function",
        pagerank: 0.5,
      },
    ],
    edges: [
      { from: "node-a", to: "node-b", kind: "SymbolReference", confidence: 1 },
    ],
    ...overrides,
  };
}

/** Wrap a snapshot in the server's `{ snapshot: ... }` envelope. */
function wrap(payload: SnapshotPayload) {
  return { snapshot: payload };
}

beforeEach(() => {
  fetchSnapshotMock.mockReset();
  useCodeGraphStore.getState().reset();
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

// ── Fetch behavior tests ──────────────────────────────────────────────────

describe("CodeGraphCanvas fetch behavior", () => {
  it("always fetches at symbol level (single fetch, no community fallback)", async () => {
    fetchSnapshotMock.mockResolvedValue(wrap(makeSnapshot()));

    render(<CodeGraphCanvas projectId="proj" />);

    await waitFor(() => {
      expect(screen.getByTestId("graph-node-count")).toHaveTextContent(
        "2 nodes",
      );
    });

    // Only one fetch — always symbol level, no community refetch.
    expect(fetchSnapshotMock).toHaveBeenCalledTimes(1);
    expect(fetchSnapshotMock).toHaveBeenCalledWith("proj", 10_000, "symbol");
  });

  it("does not refetch community even when snapshot is truncated/large", async () => {
    fetchSnapshotMock.mockResolvedValue(
      wrap(
        makeSnapshot({
          truncated: true,
          total_nodes: 12_000,
          node_cap: 10_000,
        }),
      ),
    );

    render(<CodeGraphCanvas projectId="proj" />);

    await waitFor(() => {
      expect(screen.getByTestId("graph-node-count")).toHaveTextContent(
        "2 nodes",
      );
    });

    // Still only one symbol-level fetch — LOD tiers handle large graphs.
    expect(fetchSnapshotMock).toHaveBeenCalledTimes(1);
    expect(fetchSnapshotMock).toHaveBeenCalledWith("proj", 10_000, "symbol");
  });
});

// ── LOD tier helpers ──────────────────────────────────────────────────────

describe("lodTierForZoom", () => {
  it("returns 'far' when camera ratio >= LOD_FAR_RATIO", () => {
    expect(lodTierForZoom(LOD_FAR_RATIO)).toBe("far");
    expect(lodTierForZoom(5.0)).toBe("far");
    expect(lodTierForZoom(100)).toBe("far");
  });

  it("returns 'mid' when camera ratio is between MID and FAR", () => {
    expect(lodTierForZoom(LOD_MID_RATIO)).toBe("mid");
    expect(lodTierForZoom(1.0)).toBe("mid");
    expect(lodTierForZoom(LOD_FAR_RATIO - 0.01)).toBe("mid");
  });

  it("returns 'close' when camera ratio < LOD_MID_RATIO", () => {
    expect(lodTierForZoom(LOD_MID_RATIO - 0.01)).toBe("close");
    expect(lodTierForZoom(0.1)).toBe("close");
    expect(lodTierForZoom(0.01)).toBe("close");
  });

  it("returns 'close' for non-finite camera ratio", () => {
    expect(lodTierForZoom(Infinity)).toBe("close");
    expect(lodTierForZoom(NaN)).toBe("close");
    expect(lodTierForZoom(-Infinity)).toBe("close");
  });
});

describe("isSymbolVisibleAtMidTier", () => {
  it("returns true for structural symbol kinds (class, function, etc.)", () => {
    expect(isSymbolVisibleAtMidTier("class")).toBe(true);
    expect(isSymbolVisibleAtMidTier("struct")).toBe(true);
    expect(isSymbolVisibleAtMidTier("interface")).toBe(true);
    expect(isSymbolVisibleAtMidTier("trait")).toBe(true);
    expect(isSymbolVisibleAtMidTier("enum")).toBe(true);
    expect(isSymbolVisibleAtMidTier("function")).toBe(true);
    expect(isSymbolVisibleAtMidTier("method")).toBe(true);
    expect(isSymbolVisibleAtMidTier("constructor")).toBe(true);
    expect(isSymbolVisibleAtMidTier("impl")).toBe(true);
    expect(isSymbolVisibleAtMidTier("type")).toBe(true);
  });

  it("returns false for low-priority symbol kinds (variable, import, etc.)", () => {
    expect(isSymbolVisibleAtMidTier("variable")).toBe(false);
    expect(isSymbolVisibleAtMidTier("const")).toBe(false);
    expect(isSymbolVisibleAtMidTier("static")).toBe(false);
    expect(isSymbolVisibleAtMidTier("property")).toBe(false);
    expect(isSymbolVisibleAtMidTier("field")).toBe(false);
    expect(isSymbolVisibleAtMidTier("import")).toBe(false);
    expect(isSymbolVisibleAtMidTier("other")).toBe(false);
  });

  it("returns true when symbol kind is undefined (structural node)", () => {
    expect(isSymbolVisibleAtMidTier(undefined)).toBe(true);
  });
});

describe("viewport culling", () => {
  it("classifies off-screen coordinates outside the padded viewport", () => {
    const bounds = { minX: 0, minY: 0, maxX: 100, maxY: 100 };
    expect(isInViewport(50, 50, bounds)).toBe(true);
    expect(isInViewport(250, 50, bounds)).toBe(true); // within 200px margin
    expect(isInViewport(350, 50, bounds)).toBe(false);
  });

  it("hides off-screen symbols for large-graph viewport culling", () => {
    const out = nodeReducer(
      "symbol-a",
      { kind: "symbol", symbolKind: "function", x: 1_000, y: 1_000 },
      {
        ...EMPTY_HIGHLIGHT_VIEW,
        lodTier: "close",
        viewportBounds: { minX: 0, minY: 0, maxX: 100, maxY: 100 },
      },
    );
    expect(out.hidden).toBe(true);
  });

  it("expanded regions bypass viewport culling for progressive reveal", () => {
    const out = nodeReducer(
      "symbol-a",
      { kind: "symbol", symbolKind: "function", x: 1_000, y: 1_000 },
      {
        ...EMPTY_HIGHLIGHT_VIEW,
        lodTier: "close",
        viewportBounds: { minX: 0, minY: 0, maxX: 100, maxY: 100 },
        expandedRegions: new Set(["symbol-a"]),
      },
    );
    expect(out.hidden).toBeUndefined();
  });
});

// ── Progressive expansion (expandRegion store) ────────────────────────────

describe("CodeGraphCanvas progressive expansion", () => {
  it("expandRegion adds the node to expandedRegions", () => {
    useCodeGraphStore.getState().reset();
    const { expandRegion } = useCodeGraphStore.getState();
    expect(useCodeGraphStore.getState().expandedRegions.size).toBe(0);

    expandRegion("node-a");
    expect(useCodeGraphStore.getState().expandedRegions.has("node-a")).toBe(
      true,
    );
  });

  it("expandRegion with neighbor ids adds all to expandedRegions", () => {
    useCodeGraphStore.getState().reset();
    const { expandRegion } = useCodeGraphStore.getState();

    expandRegion("node-a", ["node-b", "node-c"]);
    const regions = useCodeGraphStore.getState().expandedRegions;
    expect(regions.has("node-a")).toBe(true);
    expect(regions.has("node-b")).toBe(true);
    expect(regions.has("node-c")).toBe(true);
    expect(regions.size).toBe(3);
  });

  it("expandRegion is idempotent (same node twice does not duplicate)", () => {
    useCodeGraphStore.getState().reset();
    const { expandRegion } = useCodeGraphStore.getState();

    expandRegion("node-a");
    expandRegion("node-a");
    expect(useCodeGraphStore.getState().expandedRegions.size).toBe(1);
  });

  it("reset() clears expandedRegions (project change)", () => {
    const { expandRegion, reset } = useCodeGraphStore.getState();
    expandRegion("node-a");
    expandRegion("node-b");
    expect(useCodeGraphStore.getState().expandedRegions.size).toBe(2);

    reset();
    expect(useCodeGraphStore.getState().expandedRegions.size).toBe(0);
  });

  it("clearExpandedRegions empties the set", () => {
    const { expandRegion, clearExpandedRegions } = useCodeGraphStore.getState();
    expandRegion("node-a");
    expandRegion("node-b");

    clearExpandedRegions();
    expect(useCodeGraphStore.getState().expandedRegions.size).toBe(0);
  });
});
