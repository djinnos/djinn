import { describe, expect, it } from "vitest";

import type { SnapshotPayload } from "@/lib/codeGraphAdapter";
import {
  computeForceLayout,
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
  it("computes a deterministic force seed layout", () => {
    expect(mapEntries(computeForceLayout(fixtureSnapshot))).toEqual(
      mapEntries(computeForceLayout(fixtureSnapshot)),
    );
  });

  it("composes community-level snapshots as normal positioned nodes", () => {
    const positions = computeForceLayout(communitySnapshot);
    expect(positions.size).toBe(communitySnapshot.nodes.length);
    for (const node of communitySnapshot.nodes) {
      const position = positions.get(node.id);
      expect(position).toBeDefined();
      expect(Number.isFinite(position?.x)).toBe(true);
      expect(Number.isFinite(position?.y)).toBe(true);
    }
  });
});

function mapEntries(map: Map<string, LayoutPosition>): [string, LayoutPosition][] {
  return [...map.entries()].sort(([left], [right]) => left.localeCompare(right, "en"));
}
