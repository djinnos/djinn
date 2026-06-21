/**
 * CodeGraphCanvas semantic-zoom level selection tests.
 *
 * The canvas picks which `level` to forward to `fetchSnapshot` based on
 * the snapshot size. These tests mock the network + Sigma layers so we can
 * assert purely on the fetch-level decision logic:
 *
 *   - small graph: symbol snapshot under threshold → stays symbol, one fetch.
 *   - large graph: symbol snapshot truncated/over-threshold → refetches community.
 *
 * The `shouldFallbackToCommunity` pure helper is also unit-tested
 * directly so the boundary conditions are pinned without a render.
 *
 * Note: the canvas calls `reset()` on mount (to clear highlight state
 * from a previous project).
 */

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render, screen, waitFor } from "@testing-library/react";

import { CodeGraphCanvas, shouldFallbackToCommunity } from "./CodeGraphCanvas";
import { useCodeGraphStore } from "@/stores/codeGraphStore";
import type { SnapshotPayload } from "@/lib/codeGraphAdapter";

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

function makeSnapshot(overrides: Partial<SnapshotPayload> = {}): SnapshotPayload {
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

function makeCommunitySnapshot(): SnapshotPayload {
  return makeSnapshot({
    total_nodes: 1,
    total_edges: 0,
    nodes: [
      {
        id: "community-1",
        kind: "community",
        label: "Cluster 1",
        pagerank: 1,
        community_id: "community-1",
        member_count: 5_000,
        internal_edge_count: 1200,
      },
    ],
    edges: [],
  });
}

/** Build the raw response `fetchSnapshot` should resolve to for a given level. */
function mockResponseForLevel(
  level: "symbol" | "community",
  size: "small" | "large" = "small",
) {
  if (level === "community") return wrap(makeCommunitySnapshot());
  if (size === "large") {
    return wrap(
      makeSnapshot({
        truncated: true,
        total_nodes: 12_000,
        node_cap: 10_000,
      }),
    );
  }
  return wrap(makeSnapshot());
}

beforeEach(() => {
  fetchSnapshotMock.mockReset();
  useCodeGraphStore.getState().reset();
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

// ── Pure helper tests ─────────────────────────────────────────────────────

describe("shouldFallbackToCommunity", () => {
  it("returns false for a small non-truncated snapshot", () => {
    const snap = makeSnapshot({ truncated: false, total_nodes: 500 });
    expect(shouldFallbackToCommunity(snap, 10_000)).toBe(false);
  });

  it("returns true when the snapshot is truncated", () => {
    const snap = makeSnapshot({ truncated: true, total_nodes: 10_000 });
    expect(shouldFallbackToCommunity(snap, 10_000)).toBe(true);
  });

  it("returns true when total_nodes reaches the threshold", () => {
    const snap = makeSnapshot({ truncated: false, total_nodes: 8_000 });
    expect(shouldFallbackToCommunity(snap, 10_000)).toBe(true);
  });

  it("returns false just below the threshold", () => {
    const snap = makeSnapshot({ truncated: false, total_nodes: 7_999 });
    expect(shouldFallbackToCommunity(snap, 10_000)).toBe(false);
  });

  it("returns true when total_nodes exceeds nodeCap even if under threshold", () => {
    const snap = makeSnapshot({ truncated: false, total_nodes: 600 });
    expect(shouldFallbackToCommunity(snap, 500)).toBe(true);
  });

  it("respects a custom threshold override", () => {
    const snap = makeSnapshot({ truncated: false, total_nodes: 120 });
    expect(shouldFallbackToCommunity(snap, 10_000, 100)).toBe(true);
    expect(shouldFallbackToCommunity(snap, 10_000, 200)).toBe(false);
  });
});

// ── Canvas level-selection tests ──────────────────────────────────────────

describe("CodeGraphCanvas semantic zoom level selection", () => {
  it("auto mode keeps symbol level for small graphs (single fetch)", async () => {
    fetchSnapshotMock.mockResolvedValue(mockResponseForLevel("symbol", "small"));

    render(<CodeGraphCanvas projectId="proj" />);

    await waitFor(() => {
      expect(screen.getByTestId("semantic-zoom-level")).toHaveTextContent(
        "Symbol view",
      );
    });

    // Only one fetch — no community refetch needed.
    expect(fetchSnapshotMock).toHaveBeenCalledTimes(1);
    expect(fetchSnapshotMock).toHaveBeenCalledWith("proj", 10_000, "symbol");
  });

  it("auto mode refetches community when symbol snapshot is truncated/large", async () => {
    // First call → large symbol snapshot; second call → community snapshot.
    fetchSnapshotMock
      .mockResolvedValueOnce(mockResponseForLevel("symbol", "large"))
      .mockResolvedValueOnce(mockResponseForLevel("community"));

    render(<CodeGraphCanvas projectId="proj" />);

    await waitFor(() => {
      expect(screen.getByTestId("semantic-zoom-level")).toHaveTextContent(
        "Community view",
      );
    });

    // Two fetches: symbol then community fallback.
    expect(fetchSnapshotMock).toHaveBeenCalledTimes(2);
    expect(fetchSnapshotMock).toHaveBeenNthCalledWith(
      1,
      "proj",
      10_000,
      "symbol",
    );
    expect(fetchSnapshotMock).toHaveBeenNthCalledWith(
      2,
      "proj",
      10_000,
      "community",
    );
  });

  it("symbol snapshot under threshold stays symbol (single fetch)", async () => {
    fetchSnapshotMock.mockResolvedValue(mockResponseForLevel("symbol", "small"));

    render(<CodeGraphCanvas projectId="proj" />);

    await waitFor(() => {
      expect(screen.getByTestId("semantic-zoom-level")).toHaveTextContent(
        "Symbol view",
      );
    });

    expect(fetchSnapshotMock).toHaveBeenCalledTimes(1);
    expect(fetchSnapshotMock).toHaveBeenCalledWith("proj", 10_000, "symbol");
  });

  it("large symbol snapshot triggers community fallback", async () => {
    fetchSnapshotMock
      .mockResolvedValueOnce(mockResponseForLevel("symbol", "large"))
      .mockResolvedValueOnce(mockResponseForLevel("community"));

    render(<CodeGraphCanvas projectId="proj" />);

    await waitFor(() => {
      expect(screen.getByTestId("semantic-zoom-level")).toHaveTextContent(
        "Community view",
      );
    });

    expect(fetchSnapshotMock).toHaveBeenCalledTimes(2);
    const lastCall =
      fetchSnapshotMock.mock.calls[fetchSnapshotMock.mock.calls.length - 1];
    expect(lastCall[2]).toBe("community");
  });
});

// ── Community expand / collapse state (semantic zoom) ─────────────────────

describe("CodeGraphCanvas community expand/collapse state", () => {
  it("store tracks expanded communities by stable community_id", () => {
    useCodeGraphStore.getState().reset();
    const { expandCommunity, collapseCommunity, expandedCommunityIds } =
      useCodeGraphStore.getState();
    expect(expandedCommunityIds.size).toBe(0);

    expandCommunity("auth");
    expect(useCodeGraphStore.getState().expandedCommunityIds.has("auth")).toBe(
      true,
    );

    expandCommunity("api");
    expect(useCodeGraphStore.getState().expandedCommunityIds.size).toBe(2);

    collapseCommunity("auth");
    expect(
      useCodeGraphStore.getState().expandedCommunityIds.has("auth"),
    ).toBe(false);
    expect(useCodeGraphStore.getState().expandedCommunityIds.has("api")).toBe(
      true,
    );
  });

  it("expandCommunity and collapseCommunity are idempotent", () => {
    useCodeGraphStore.getState().reset();
    const { expandCommunity, collapseCommunity } = useCodeGraphStore.getState();

    expandCommunity("auth");
    expandCommunity("auth");
    expect(useCodeGraphStore.getState().expandedCommunityIds.size).toBe(1);

    collapseCommunity("auth");
    collapseCommunity("auth");
    expect(
      useCodeGraphStore.getState().expandedCommunityIds.has("auth"),
    ).toBe(false);
  });

  it("reset() clears expanded communities (project change)", () => {
    const { expandCommunity, reset } = useCodeGraphStore.getState();
    expandCommunity("auth");
    expandCommunity("api");
    expect(useCodeGraphStore.getState().expandedCommunityIds.size).toBe(2);

    reset();
    expect(useCodeGraphStore.getState().expandedCommunityIds.size).toBe(0);
  });

  it("clearExpandedCommunities empties the set", () => {
    const { expandCommunity, clearExpandedCommunities } =
      useCodeGraphStore.getState();
    expandCommunity("auth");
    expandCommunity("api");

    clearExpandedCommunities();
    expect(useCodeGraphStore.getState().expandedCommunityIds.size).toBe(0);
  });

  it("isDoubleClick detects a double-click within the interval", async () => {
    const { isDoubleClick } = await import("@/lib/codeGraphAdapter");
    const prev = { nodeId: "community:auth", at: 1000 };
    expect(isDoubleClick(prev, "community:auth", 1200)).toBe(true);
    expect(isDoubleClick(prev, "community:auth", 1000)).toBe(true);
    expect(isDoubleClick(prev, "community:api", 1200)).toBe(false);
  });
});
