/**
 * CodeGraphCanvas semantic-zoom level selection tests.
 *
 * The canvas picks which `level` to forward to `fetchSnapshot` based on
 * the store's `semanticZoomMode` and (in auto mode) the snapshot size.
 * These tests mock the network + Sigma layers so we can assert purely on
 * the fetch-level decision logic:
 *
 *   - small-auto: symbol snapshot under threshold → stays symbol, one fetch.
 *   - large-auto: symbol snapshot truncated/over-threshold → refetches community.
 *   - forced-symbol: always symbol, never falls back.
 *   - forced-community: always community, single fetch.
 *
 * The `shouldFallbackToCommunity` pure helper is also unit-tested
 * directly so the boundary conditions are pinned without a render.
 *
 * Note: the canvas calls `reset()` on mount (to clear highlight state
 * from a previous project), which resets `semanticZoomMode` to the
 * default "auto". Tests that need a forced mode therefore set it
 * *after* the initial mount settle and then wait for the refetch.
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

  it("forced symbol mode fetches symbol only and never falls back", async () => {
    // Initial auto-mode mount fetches a small symbol snapshot (no
    // fallback). After switching to forced symbol, the mock returns a
    // large/truncated symbol snapshot — forced mode must NOT fall back
    // to community even though the snapshot is capped.
    fetchSnapshotMock.mockResolvedValue(mockResponseForLevel("symbol", "small"));

    render(<CodeGraphCanvas projectId="proj" />);

    // Wait for the initial auto-mode fetch to complete.
    await waitFor(() => {
      expect(screen.getByTestId("semantic-zoom-level")).toBeInTheDocument();
    });

    // The canvas resets semanticZoomMode to "auto" on mount; switch to
    // forced symbol after the initial settle so the refetch honors it.
    // Now return a large snapshot so we can verify no community fallback.
    fetchSnapshotMock.mockResolvedValue(mockResponseForLevel("symbol", "large"));
    act(() => {
      useCodeGraphStore.getState().setSemanticZoomMode("symbol");
    });

    await waitFor(() => {
      expect(screen.getByTestId("semantic-zoom-level")).toHaveTextContent(
        "Symbol view",
      );
    });

    // The most recent fetch must be at symbol level.
    const lastCall =
      fetchSnapshotMock.mock.calls[fetchSnapshotMock.mock.calls.length - 1];
    expect(lastCall[2]).toBe("symbol");

    // No community fetch should ever have been issued.
    const communityCalls = fetchSnapshotMock.mock.calls.filter(
      (c) => c[2] === "community",
    );
    expect(communityCalls).toHaveLength(0);
  });

  it("forced community mode fetches community immediately", async () => {
    fetchSnapshotMock.mockResolvedValue(mockResponseForLevel("community"));

    render(<CodeGraphCanvas projectId="proj" />);

    // Wait for the initial auto-mode fetch to complete.
    await waitFor(() => {
      expect(screen.getByTestId("semantic-zoom-level")).toBeInTheDocument();
    });

    // The canvas resets semanticZoomMode to "auto" on mount; switch to
    // forced community after the initial settle.
    act(() => {
      useCodeGraphStore.getState().setSemanticZoomMode("community");
    });

    await waitFor(() => {
      expect(screen.getByTestId("semantic-zoom-level")).toHaveTextContent(
        "Community view",
      );
    });

    // The most recent fetch must be at community level.
    const lastCall =
      fetchSnapshotMock.mock.calls[fetchSnapshotMock.mock.calls.length - 1];
    expect(lastCall[2]).toBe("community");
  });

  it("changing the toolbar mode triggers a safe refetch at the new level", async () => {
    // Start in auto with a small graph → symbol level.
    fetchSnapshotMock.mockResolvedValue(mockResponseForLevel("symbol", "small"));

    render(<CodeGraphCanvas projectId="proj" />);

    await waitFor(() => {
      expect(screen.getByTestId("semantic-zoom-level")).toHaveTextContent(
        "Symbol view",
      );
    });

    // Switch to forced community → refetch at community level.
    fetchSnapshotMock.mockResolvedValue(mockResponseForLevel("community"));
    act(() => {
      useCodeGraphStore.getState().setSemanticZoomMode("community");
    });

    await waitFor(() => {
      expect(screen.getByTestId("semantic-zoom-level")).toHaveTextContent(
        "Community view",
      );
    });

    const lastCall =
      fetchSnapshotMock.mock.calls[fetchSnapshotMock.mock.calls.length - 1];
    expect(lastCall[2]).toBe("community");
  });
});
