/**
 * Hotspot leaderboard derivation (`snapshotHotspots`): per-file worst
 * cognitive complexity joined with qoxm co-change coupling (partner ids,
 * max score, last-co-change day decoded from the edge reason), ranked by
 * complexity × (1 + Σ coupling).
 */

import { describe, expect, it } from "vitest";

import type { SnapshotPayload } from "@/lib/codeGraphAdapter";
import { snapshotHotspots } from "@/lib/codeGraphGalaxyAdapter";

type NodeOverride = Record<string, unknown>;
type EdgeOverride = Record<string, unknown>;

function snapshot(
  nodes: NodeOverride[],
  edges: EdgeOverride[],
): SnapshotPayload {
  return {
    project_id: "proj-1",
    git_head: "deadbeef",
    generated_at: "2026-07-15T00:00:00Z",
    truncated: false,
    total_nodes: nodes.length,
    total_edges: edges.length,
    node_cap: 1000,
    nodes,
    edges,
  } as unknown as SnapshotPayload;
}

function file(
  id: string,
  path: string,
  extra: NodeOverride = {},
): NodeOverride {
  return {
    id,
    kind: "file",
    label: path,
    file_path: path,
    pagerank: 0.1,
    ...extra,
  };
}

function fn(
  id: string,
  label: string,
  fileId: string,
  cognitive: number,
): {
  node: NodeOverride;
  edge: EdgeOverride;
} {
  return {
    node: {
      id,
      kind: "symbol",
      label,
      symbol_kind: "function",
      pagerank: 0.1,
      cognitive,
    },
    edge: { from: fileId, to: id, kind: "ContainsDefinition", confidence: 1 },
  };
}

describe("snapshotHotspots", () => {
  it("rolls function complexity up to the containing file and keeps the worst", () => {
    const tame = fn("sym:tame", "tame", "file:a.rs", 3);
    const monster = fn("sym:monster", "monster", "file:a.rs", 31);
    const snap = snapshot(
      [file("file:a.rs", "src/a.rs"), tame.node, monster.node],
      [tame.edge, monster.edge],
    );

    const rows = snapshotHotspots(snap);

    expect(rows).toHaveLength(1);
    expect(rows[0]).toMatchObject({
      fileId: "file:a.rs",
      path: "src/a.rs",
      complexity: 31,
      worstSymbol: "monster",
      functionCount: 2,
      partnerIds: [],
      coupling: 0,
      score: 31,
    });
  });

  it("joins co-change coupling and ranks by complexity × (1 + Σ coupling)", () => {
    const fa = fn("sym:a", "handler_a", "file:a.rs", 10);
    const fb = fn("sym:b", "handler_b", "file:b.rs", 12);
    const snap = snapshot(
      [
        file("file:a.rs", "src/a.rs"),
        file("file:b.rs", "src/b.rs"),
        fa.node,
        fb.node,
      ],
      [
        fa.edge,
        fb.edge,
        // a co-changes with b (score 0.8, last day 20000).
        {
          from: "file:a.rs",
          to: "file:b.rs",
          kind: "CoChangedWith",
          confidence: 0.8,
          reason: "cochange;last_day=20000",
        },
      ],
    );

    const rows = snapshotHotspots(snap);

    expect(rows).toHaveLength(2);
    // b (12 × 1.8 = 21.6) outranks a (10 × 1.8 = 18).
    expect(rows[0].fileId).toBe("file:b.rs");
    expect(rows[0].score).toBeCloseTo(12 * 1.8);
    // Coupling is symmetric — both ends carry the partner and the day.
    for (const row of rows) {
      expect(row.coupling).toBeCloseTo(0.8);
      expect(row.lastCoChangeDay).toBe(20000);
      expect(row.partnerIds).toHaveLength(1);
    }
    expect(rows[1].partnerIds).toEqual(["file:b.rs"]);
  });

  it("excludes files with no scored functions and zero-complexity files", () => {
    const zero = fn("sym:zero", "zero", "file:a.rs", 0);
    const snap = snapshot(
      [
        file("file:a.rs", "src/a.rs"),
        // Coupled but unscored: churn on simple code is not a hotspot.
        file("file:b.rs", "src/b.rs"),
        zero.node,
      ],
      [
        zero.edge,
        {
          from: "file:a.rs",
          to: "file:b.rs",
          kind: "CoChangedWith",
          confidence: 0.9,
          reason: "cochange;last_day=20000",
        },
      ],
    );

    expect(snapshotHotspots(snap)).toHaveLength(0);
  });

  it("tolerates a missing/malformed co-change reason (no recency, coupling kept)", () => {
    const fa = fn("sym:a", "handler_a", "file:a.rs", 20);
    const snap = snapshot(
      [file("file:a.rs", "src/a.rs"), file("file:b.rs", "src/b.rs"), fa.node],
      [
        fa.edge,
        {
          from: "file:a.rs",
          to: "file:b.rs",
          kind: "CoChangedWith",
          confidence: 0.5,
        },
      ],
    );

    const rows = snapshotHotspots(snap);

    expect(rows).toHaveLength(1);
    expect(rows[0].coupling).toBeCloseTo(0.5);
    expect(rows[0].lastCoChangeDay).toBeUndefined();
    expect(rows[0].score).toBeCloseTo(20 * 1.5);
  });
});
