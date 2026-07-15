/**
 * Proposal lmkv — server-shipped galaxy layout fast path.
 *
 * When the `code_graph snapshot` carries per-node galaxy coordinates
 * (gx/gy/gz), `snapshotToGalaxy` must use them verbatim, flag the result as
 * `serverPositioned`, and NEVER run the client force layout. When they're
 * absent (legacy blobs, the Storybook fixture) it must fall back to the
 * existing layout path unchanged.
 */

import { describe, expect, it, vi } from "vitest";

import type { SnapshotPayload } from "@/lib/codeGraphAdapter";

// Spy on the force layout so we can assert it is not invoked on the fast path.
const layoutGalaxy = vi.fn();
vi.mock("@/components/galaxy/galaxyLayout", () => ({
  layoutGalaxy: (...args: unknown[]) => layoutGalaxy(...args),
}));

// Imported after the mock is registered.
const { snapshotToGalaxy } = await import("@/lib/codeGraphGalaxyAdapter");

function baseSnapshot(overrides: Partial<SnapshotPayload["nodes"][number]>[]): SnapshotPayload {
  const nodes = [
    {
      id: "file:a/b/one.rs",
      kind: "file",
      label: "a/b/one.rs",
      pagerank: 0.5,
      file_path: "a/b/one.rs",
      workspace: "root",
      ...overrides[0],
    },
    {
      id: "file:a/b/two.rs",
      kind: "file",
      label: "a/b/two.rs",
      pagerank: 0.4,
      file_path: "a/b/two.rs",
      workspace: "root",
      ...overrides[1],
    },
  ];
  return {
    project_id: "proj-1",
    git_head: "deadbeef",
    generated_at: "2026-07-15T00:00:00Z",
    truncated: false,
    total_nodes: nodes.length,
    total_edges: 1,
    node_cap: 1000,
    nodes,
    edges: [
      { from: "file:a/b/one.rs", to: "file:a/b/two.rs", kind: "FileReference", confidence: 1 },
    ],
  } as unknown as SnapshotPayload;
}

describe("snapshotToGalaxy server layout fast path", () => {
  it("uses server coordinates and skips the layout when every node is positioned", () => {
    layoutGalaxy.mockClear();
    const snapshot = baseSnapshot([
      { gx: 10, gy: 20, gz: 30, degree: 7 },
      { gx: -5, gy: -6, gz: -7, degree: 3 },
    ]);

    // Default options (layout would otherwise run) — the fast path must win.
    const data = snapshotToGalaxy(snapshot);

    expect(data.serverPositioned).toBe(true);
    expect(layoutGalaxy).not.toHaveBeenCalled();

    const one = data.nodes.find((n) => n.id === "file:a/b/one.rs")!;
    expect([one.x, one.y, one.z]).toEqual([10, 20, 30]);
    // Server-shipped degree is used verbatim.
    expect(one.degree).toBe(7);
  });

  it("falls back to the client layout when server coordinates are absent", () => {
    layoutGalaxy.mockClear();
    const snapshot = baseSnapshot([{}, {}]);

    const data = snapshotToGalaxy(snapshot);

    expect(data.serverPositioned).toBe(false);
    expect(layoutGalaxy).toHaveBeenCalledTimes(1);
    // Degree falls back to the collapsed-edge count (one FileReference edge).
    const one = data.nodes.find((n) => n.id === "file:a/b/one.rs")!;
    expect(one.degree).toBe(1);
  });

  it("does not take the fast path when only some nodes are positioned", () => {
    layoutGalaxy.mockClear();
    const snapshot = baseSnapshot([
      { gx: 1, gy: 2, gz: 3 },
      {}, // missing coordinates
    ]);

    const data = snapshotToGalaxy(snapshot, { layout: false });

    expect(data.serverPositioned).toBe(false);
    // layout:false means neither the fast path nor the inline layout runs here;
    // GalaxyView will run the worker.
    expect(layoutGalaxy).not.toHaveBeenCalled();
  });
});
