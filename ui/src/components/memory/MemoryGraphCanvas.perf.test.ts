import { describe, expect, it } from "vitest";

import { buildMemoryGraphDisk } from "./MemoryGraphCanvas";
import type { MemoryGraphOutput } from "@/api/generated/mcp-tools.gen";
import { validLifecycleResponse } from "@/lib/__fixtures__/memoryGraphLifecycle";
import { parseMemoryGraphResponse } from "@/lib/memoryGraphAdapter";

const ACTIVE_OR_PROPOSAL_NODES = 1_500;
const LIFECYCLE_GHOST_NODES = 500;
const WARMUP_RUNS = 2;
const MEASURED_RUNS = 20;

function median(samples: readonly number[]): number {
  const sorted = [...samples].sort((a, b) => a - b);
  const middle = sorted.length / 2;
  return (sorted[middle - 1] + sorted[middle]) / 2;
}

/** Build raw MCP-shaped data once; it is deliberately outside the timed region. */
function activeProposalGraph(): MemoryGraphOutput {
  const nodes: MemoryGraphOutput["nodes"] = Array.from({ length: ACTIVE_OR_PROPOSAL_NODES }, (_, index) => ({
    id: `active-${index}`,
    permalink: `notes/active-${index}`,
    title: `Active or proposal note ${index}`,
    note_type: index % 2 === 0 ? "adr" : "research",
    folder: "notes",
    connection_count: index % 9,
    is_orphan: false,
    status: "active",
    ...(index % 5 === 0 ? { entity_type: "proposal" } : {}),
    created_at: new Date(Date.UTC(2026, 0, 1 + (index % 180))).toISOString(),
  }));
  const edges = Array.from({ length: ACTIVE_OR_PROPOSAL_NODES - 1 }, (_, index) => ({
    source_id: `active-${index + 1}`,
    target_id: `active-${index}`,
    raw_text: `Active or proposal note ${index}`,
  }));
  return { nodes, edges, typed_edges: [] };
}

function lifecycleGhosts(): MemoryGraphOutput["nodes"] {
  const archivedFixture = validLifecycleResponse.nodes.find((node) => node.status === "archived")!;
  const deprecatedFixture = validLifecycleResponse.nodes.find((node) => node.status === "deprecated")!;
  return Array.from({ length: LIFECYCLE_GHOST_NODES }, (_, index) => ({
    id: `ghost-${index}`,
    permalink: `notes/ghost-${index}`,
    title: `${index % 2 === 0 ? archivedFixture.title : deprecatedFixture.title} ${index}`,
    note_type: index % 2 === 0 ? "reference" : "pitfall",
    folder: "notes",
    connection_count: index % 5,
    is_orphan: false,
    status: index % 2 === 0 ? "archived" : "deprecated",
    lifecycle_changed_at: "2026-07-20T12:00:00Z",
    created_at: new Date(Date.UTC(2026, 0, 1 + (index % 180))).toISOString(),
  }));
}

function parseAndBuild(raw: unknown) {
  const parsed = parseMemoryGraphResponse(raw);
  if (parsed === null) throw new Error("benchmark payload must parse through the shared adapter");
  return buildMemoryGraphDisk(parsed);
}

function measureParseAndBuild(raw: unknown): number[] {
  // Parsing, model building, and their JIT warmup are intentional; React,
  // browser mounting, responder setup, and fixture construction are not.
  for (let index = 0; index < WARMUP_RUNS; index += 1) parseAndBuild(raw);
  return Array.from({ length: MEASURED_RUNS }, () => {
    const startedAt = performance.now();
    parseAndBuild(raw);
    return performance.now() - startedAt;
  });
}

describe("MemoryGraphCanvas parse/build performance", () => {
  it("keeps 500 lifecycle ghosts within the in-process model budget", () => {
    const activeOnly = activeProposalGraph();
    const lifecycleInclusive = {
      ...activeOnly,
      nodes: [...activeOnly.nodes, ...lifecycleGhosts()],
      edges: [
        ...activeOnly.edges,
        ...Array.from({ length: LIFECYCLE_GHOST_NODES }, (_, index) => ({
          source_id: `active-${index}`,
          target_id: `ghost-${index}`,
          raw_text: `Lifecycle ghost ${index}`,
        })),
      ],
    };

    // Validate shape outside the measured section so benchmark samples contain
    // only adapter parse + pure disk/model construction work.
    expect(parseAndBuild(activeOnly).nodes).toHaveLength(ACTIVE_OR_PROPOSAL_NODES);
    expect(parseAndBuild(lifecycleInclusive).nodes).toHaveLength(ACTIVE_OR_PROPOSAL_NODES + LIFECYCLE_GHOST_NODES);

    const activeMedian = median(measureParseAndBuild(activeOnly));
    const lifecycleMedian = median(measureParseAndBuild(lifecycleInclusive));

    expect(lifecycleMedian).toBeLessThanOrEqual(activeMedian * 1.2);
    expect(lifecycleMedian).toBeLessThanOrEqual(100);
  });
});
