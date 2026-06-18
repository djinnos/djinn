import { describe, expect, it } from "vitest";

import type { SnapshotPayload } from "@/lib/codeGraphAdapter";
import {
  computeForceLayout,
  computeRadialLayout,
  computeSequentialLayout,
  type LayoutPosition,
} from "@/lib/codeGraphLayouts";

const fixtureSnapshot: SnapshotPayload = {
  project_id: "proj-layout-test",
  git_head: "deadbeef",
  generated_at: "2026-06-18T00:00:00Z",
  truncated: false,
  total_nodes: 7,
  total_edges: 5,
  node_cap: 2_000,
  nodes: [
    {
      id: "folder:src",
      kind: "folder",
      label: "src",
      pagerank: 0.2,
    },
    {
      id: "file:src/main.rs",
      kind: "file",
      label: "main.rs",
      pagerank: 0.4,
    },
    {
      id: "file:src/user.rs",
      kind: "file",
      label: "user.rs",
      pagerank: 0.1,
    },
    {
      id: "symbol:main",
      kind: "symbol",
      label: "main",
      symbol_kind: "function",
      file_path: "src/main.rs",
      pagerank: 0.95,
    },
    {
      id: "symbol:new",
      kind: "symbol",
      label: "new",
      symbol_kind: "method",
      file_path: "src/user.rs",
      pagerank: 0.3,
    },
    {
      id: "symbol:User",
      kind: "symbol",
      label: "User",
      symbol_kind: "class",
      file_path: "src/user.rs",
      pagerank: 0.8,
    },
    {
      id: "symbol:User.name",
      kind: "symbol",
      label: "name",
      symbol_kind: "field",
      file_path: "src/user.rs",
      pagerank: 0.25,
    },
  ],
  edges: [
    {
      from: "folder:src",
      to: "file:src/main.rs",
      kind: "FileReference",
      confidence: 1,
    },
    {
      from: "folder:src",
      to: "file:src/user.rs",
      kind: "FileReference",
      confidence: 1,
    },
    {
      from: "file:src/main.rs",
      to: "symbol:main",
      kind: "ContainsDefinition",
      confidence: 1,
    },
    {
      from: "file:src/user.rs",
      to: "symbol:User",
      kind: "ContainsDefinition",
      confidence: 1,
    },
    {
      from: "symbol:main",
      to: "symbol:User",
      kind: "SymbolReference",
      confidence: 0.8,
    },
  ],
};

const disconnectedSnapshot: SnapshotPayload = {
  ...fixtureSnapshot,
  total_nodes: 4,
  total_edges: 2,
  nodes: [
    {
      id: "file:a.ts",
      kind: "file",
      label: "a.ts",
      pagerank: 1,
    },
    {
      id: "symbol:a",
      kind: "symbol",
      label: "a",
      symbol_kind: "function",
      file_path: "a.ts",
      pagerank: 0.5,
    },
    {
      id: "file:b.ts",
      kind: "file",
      label: "b.ts",
      pagerank: 0.2,
    },
    {
      id: "symbol:b",
      kind: "symbol",
      label: "b",
      symbol_kind: "function",
      file_path: "b.ts",
      pagerank: 0.1,
    },
  ],
  edges: [
    {
      from: "file:a.ts",
      to: "symbol:a",
      kind: "ContainsDefinition",
      confidence: 1,
    },
    {
      from: "file:b.ts",
      to: "symbol:b",
      kind: "ContainsDefinition",
      confidence: 1,
    },
  ],
};

const communitySnapshot: SnapshotPayload = {
  project_id: "proj-community-layout-test",
  git_head: "cafebabe",
  generated_at: "2026-06-18T00:00:00Z",
  truncated: false,
  total_nodes: 3,
  total_edges: 2,
  node_cap: 2_000,
  nodes: [
    {
      id: "community:auth",
      kind: "community",
      label: "auth",
      pagerank: 0.7,
      community_id: "auth",
      member_count: 12,
      internal_edge_count: 20,
    },
    {
      id: "community:api",
      kind: "community",
      label: "api",
      pagerank: 0.9,
      community_id: "api",
      member_count: 8,
      internal_edge_count: 10,
    },
    {
      id: "community:ui",
      kind: "community",
      label: "ui",
      pagerank: 0.3,
      community_id: "ui",
      member_count: 6,
      internal_edge_count: 5,
    },
  ],
  edges: [
    {
      from: "community:api",
      to: "community:auth",
      kind: "CommunityDependsOn",
      confidence: 1,
    },
    {
      from: "community:ui",
      to: "community:api",
      kind: "CommunityDependsOn",
      confidence: 1,
    },
  ],
};

describe("codeGraphLayouts", () => {
  it("computes deterministic sequential and radial layouts", () => {
    expect(mapEntries(computeSequentialLayout(fixtureSnapshot))).toEqual(
      mapEntries(computeSequentialLayout(fixtureSnapshot)),
    );
    expect(mapEntries(computeRadialLayout(fixtureSnapshot, "folder:src"))).toEqual(
      mapEntries(computeRadialLayout(fixtureSnapshot, "folder:src")),
    );
  });

  it("computes a deterministic force seed layout", () => {
    expect(mapEntries(computeForceLayout(fixtureSnapshot))).toEqual(
      mapEntries(computeForceLayout(fixtureSnapshot)),
    );
  });

  it("places folders above files and files above symbols in sequential layout", () => {
    const positions = computeSequentialLayout(fixtureSnapshot);
    const folderYs = ysForKind(fixtureSnapshot, positions, "folder");
    const fileYs = ysForKind(fixtureSnapshot, positions, "file");
    const symbolYs = ysForKind(fixtureSnapshot, positions, "symbol");

    expect(Math.max(...folderYs)).toBeLessThan(Math.min(...fileYs));
    expect(Math.max(...fileYs)).toBeLessThan(Math.min(...symbolYs));
  });

  it("orders function and method symbols above type symbols above field symbols", () => {
    const positions = computeSequentialLayout(fixtureSnapshot);
    const functionLikeYs = ysForSymbolKinds(fixtureSnapshot, positions, [
      "function",
      "method",
    ]);
    const typeYs = ysForSymbolKinds(fixtureSnapshot, positions, ["class"]);
    const fieldYs = ysForSymbolKinds(fixtureSnapshot, positions, ["field"]);

    expect(Math.max(...functionLikeYs)).toBeLessThan(Math.min(...typeYs));
    expect(Math.max(...typeYs)).toBeLessThan(Math.min(...fieldYs));
  });

  it("falls back to the highest-pagerank node as the radial focal root", () => {
    const positions = computeRadialLayout(fixtureSnapshot, null);
    expect(positions.get("symbol:main")).toEqual({ x: 0, y: 0 });
  });

  it("assigns finite outer-shell positions to disconnected radial nodes", () => {
    const positions = computeRadialLayout(disconnectedSnapshot, "file:a.ts");
    expect(positions.size).toBe(disconnectedSnapshot.nodes.length);

    for (const node of disconnectedSnapshot.nodes) {
      const position = positions.get(node.id);
      expect(position).toBeDefined();
      expect(Number.isFinite(position?.x)).toBe(true);
      expect(Number.isFinite(position?.y)).toBe(true);
    }

    expect(positions.get("file:b.ts")).not.toEqual({ x: 0, y: 0 });
    expect(positions.get("symbol:b")).not.toEqual({ x: 0, y: 0 });
  });

  it("composes community-level snapshots as normal positioned nodes", () => {
    for (const compute of [
      computeForceLayout,
      computeSequentialLayout,
      (snapshot: SnapshotPayload) => computeRadialLayout(snapshot, null),
    ]) {
      const positions = compute(communitySnapshot);
      expect(positions.size).toBe(communitySnapshot.nodes.length);
      for (const node of communitySnapshot.nodes) {
        const position = positions.get(node.id);
        expect(position).toBeDefined();
        expect(Number.isFinite(position?.x)).toBe(true);
        expect(Number.isFinite(position?.y)).toBe(true);
      }
    }
  });
});

function mapEntries(map: Map<string, LayoutPosition>): [string, LayoutPosition][] {
  return [...map.entries()].sort(([left], [right]) => left.localeCompare(right, "en"));
}

function ysForKind(
  snapshot: SnapshotPayload,
  positions: Map<string, LayoutPosition>,
  kind: "folder" | "file" | "symbol" | "community",
): number[] {
  return snapshot.nodes
    .filter((node) => node.kind === kind)
    .map((node) => positions.get(node.id)?.y ?? Number.NaN);
}

function ysForSymbolKinds(
  snapshot: SnapshotPayload,
  positions: Map<string, LayoutPosition>,
  symbolKinds: string[],
): number[] {
  const symbolKindSet = new Set(symbolKinds);
  return snapshot.nodes
    .filter(
      (node) =>
        node.kind === "symbol" && symbolKindSet.has(node.symbol_kind ?? "other"),
    )
    .map((node) => positions.get(node.id)?.y ?? Number.NaN);
}
