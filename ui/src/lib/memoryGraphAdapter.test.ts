/**
 * memoryGraphAdapter — unit tests for the pure adapter surface AND integration
 * tests for the memory graph → community clustering → per-community attraction
 * wiring.
 *
 * Pure adapter tests (task 2chl AC):
 *   - Determinism: same input + seed → identical node attrs.
 *   - Palette mapping: each `note_type` resolves to the expected color;
 *     orphan override wins regardless of type; unknown types fall back.
 *   - Size floor/ceiling: `connection_count=0` ≥ MIN; high count capped.
 *   - Edge kind classification: `broken_targets` produces `"broken"` stub
 *     edges with raw text; resolved edges are `"wikilink"`; unknown-target
 *     edges dropped.
 *   - Empty / missing-field inputs: empty nodes → empty graph; missing
 *     `note_type` → default color; `broken_targets: []` → no broken edges.
 *
 * Community integration tests (ewfa epic):
 *   1. `buildClusteredMemoryGraph` builds the graphology graph AND runs
 *      `clusterMemoryCommunities`, writing `communityId`/`communityLabel`
 *      onto clustered nodes while leaving singletons/unclustered notes
 *      without those attributes.
 *   2. The single-node / no-community path is a no-op for the per-community
 *      attraction pass.
 *   3. The community metadata map can be fed straight into
 *      `applyPerCommunityAttraction` and produces deterministic centers.
 */

import { describe, expect, it } from "vitest";

import type { MemoryGraphOutput } from "@/api/generated/mcp-tools.gen";
import {
  buildClusteredMemoryGraph,
  buildMemoryGraph,
  buildMemoryGraphFromPayload,
  applyMemoryCommunities,
  parseMemoryGraphResponse,
  colorForNote,
  sizeForConnectionCount,
  createSeededRandom,
  PALETTE,
  DEFAULT_COLOR,
  ORPHAN_OVERRIDE,
  MIN_NODE_SIZE,
  MAX_NODE_SIZE,
  MEMORY_COMMUNITY_ID_ATTRIBUTE,
  MEMORY_COMMUNITY_LABEL_ATTRIBUTE,
} from "@/lib/memoryGraphAdapter";
import { applyPerCommunityAttraction } from "@/lib/perCommunityAttraction";

// ── Fixture helpers ─────────────────────────────────────────────────────────

function makePayload(overrides: Partial<MemoryGraphOutput> = {}): MemoryGraphOutput {
  return {
    nodes: [],
    edges: [],
    ...overrides,
  };
}

function makeNode(overrides: Partial<MemoryGraphOutput["nodes"][number]> = {}): MemoryGraphOutput["nodes"][number] {
  return {
    id: "n1",
    title: "Test Note",
    permalink: "notes/test-note",
    folder: "notes",
    note_type: "adr",
    connection_count: 1,
    is_orphan: false,
    broken_targets: [],
    ...overrides,
  };
}

// ── Determinism ─────────────────────────────────────────────────────────────

describe("buildMemoryGraph — determinism", () => {
  it("produces identical node attributes (id, x, y, color, size) for the same input + seed", () => {
    const payload = makePayload({
      nodes: [
        makeNode({ id: "a", note_type: "adr", connection_count: 4 }),
        makeNode({ id: "b", note_type: "pattern", connection_count: 9 }),
        makeNode({ id: "c", note_type: "case", connection_count: 0 }),
      ],
      edges: [
        { source_id: "a", target_id: "b", raw_text: "[[b]]" },
      ],
    });

    const g1 = buildMemoryGraph(payload, { seed: 42 });
    const g2 = buildMemoryGraph(payload, { seed: 42 });

    for (const id of ["a", "b", "c"]) {
      const a1 = g1.getNodeAttributes(id);
      const a2 = g2.getNodeAttributes(id);
      expect(a1.x).toBe(a2.x);
      expect(a1.y).toBe(a2.y);
      expect(a1.color).toBe(a2.color);
      expect(a1.size).toBe(a2.size);
      expect(a1.note_type).toBe(a2.note_type);
    }
    expect(g1.order).toBe(g2.order);
    expect(g1.size).toBe(g2.size);
  });

  it("produces different x/y for different seeds (jitter is seed-dependent)", () => {
    const payload = makePayload({
      nodes: [makeNode({ id: "a" })],
    });
    const g1 = buildMemoryGraph(payload, { seed: 1 });
    const g2 = buildMemoryGraph(payload, { seed: 999 });
    // The golden-angle base is the same, but the jitter differs by seed.
    expect(g1.getNodeAttributes("a").x).not.toBe(g2.getNodeAttributes("a").x);
  });

  it("uses a default seed when none is provided", () => {
    const payload = makePayload({
      nodes: [makeNode({ id: "a" })],
    });
    const g1 = buildMemoryGraph(payload);
    const g2 = buildMemoryGraph(payload, { seed: 1 });
    expect(g1.getNodeAttributes("a").x).toBe(g2.getNodeAttributes("a").x);
  });
});

// ── Color palette mapping ───────────────────────────────────────────────────

describe("buildMemoryGraph — color palette mapping", () => {
  it("maps each known note_type to its palette color", () => {
    for (const [noteType, expectedColor] of Object.entries(PALETTE)) {
      const graph = buildMemoryGraph(
        makePayload({ nodes: [makeNode({ id: noteType, note_type: noteType })] }),
      );
      expect(graph.getNodeAttributes(noteType).color).toBe(expectedColor);
    }
  });

  it("falls back to DEFAULT_COLOR for unknown note types", () => {
    const graph = buildMemoryGraph(
      makePayload({ nodes: [makeNode({ id: "x", note_type: "some-unknown-type" })] }),
    );
    expect(graph.getNodeAttributes("x").color).toBe(DEFAULT_COLOR);
  });

  it("falls back to DEFAULT_COLOR for empty note_type", () => {
    const graph = buildMemoryGraph(
      makePayload({ nodes: [makeNode({ id: "x", note_type: "" })] }),
    );
    expect(graph.getNodeAttributes("x").color).toBe(DEFAULT_COLOR);
  });

  it("overrides to ORPHAN_OVERRIDE when is_orphan is true, regardless of note_type", () => {
    for (const noteType of Object.keys(PALETTE)) {
      const graph = buildMemoryGraph(
        makePayload({
          nodes: [makeNode({ id: noteType, note_type: noteType, is_orphan: true })],
        }),
      );
      expect(graph.getNodeAttributes(noteType).color).toBe(ORPHAN_OVERRIDE);
    }
    // Unknown type + orphan also red.
    const graph = buildMemoryGraph(
      makePayload({
        nodes: [makeNode({ id: "x", note_type: "mystery", is_orphan: true })],
      }),
    );
    expect(graph.getNodeAttributes("x").color).toBe(ORPHAN_OVERRIDE);
  });

  it("colorForNote helper matches the graph attribute", () => {
    expect(colorForNote("adr", false)).toBe(PALETTE.adr);
    expect(colorForNote("adr", true)).toBe(ORPHAN_OVERRIDE);
    expect(colorForNote("nope", false)).toBe(DEFAULT_COLOR);
  });

  // ── Enrichment node types (epic diei: entity / claim) ─────────────────────
  // These are the note_type values task qp5s established for LLM-enrichment
  // nodes. They must render with a color distinct from each other and from
  // every pre-existing palette hue so the graph can tell them apart.

  it("maps enrichment `entity` and `claim` note types to distinct palette colors", () => {
    const graph = buildMemoryGraph(
      makePayload({
        nodes: [
          makeNode({ id: "entity-node", note_type: "entity" }),
          makeNode({ id: "claim-node", note_type: "claim" }),
        ],
      }),
    );
    const entityColor = graph.getNodeAttributes("entity-node").color;
    const claimColor = graph.getNodeAttributes("claim-node").color;

    // Each maps to its own palette entry…
    expect(entityColor).toBe(PALETTE.entity);
    expect(claimColor).toBe(PALETTE.claim);
    // …and the two are visually distinct from each other.
    expect(entityColor).not.toBe(claimColor);
  });

  it("enrichment entity/claim colors are distinct from every pre-existing palette hue", () => {
    const preExisting = new Set(
      ["adr", "pattern", "case", "pitfall", "research", "reference"].map(
        (t) => PALETTE[t],
      ),
    );
    expect(preExisting.has(PALETTE.entity)).toBe(false);
    expect(preExisting.has(PALETTE.claim)).toBe(false);
  });

  it("renders an entity node, a claim node, and an existing note type together without color collisions", () => {
    const graph = buildMemoryGraph(
      makePayload({
        nodes: [
          makeNode({ id: "a", note_type: "adr" }),
          makeNode({ id: "e", note_type: "entity" }),
          makeNode({ id: "c", note_type: "claim" }),
        ],
      }),
    );
    const colors = new Set([
      graph.getNodeAttributes("a").color,
      graph.getNodeAttributes("e").color,
      graph.getNodeAttributes("c").color,
    ]);
    // All three distinct.
    expect(colors.size).toBe(3);
  });

  it("preserves entity/claim note_type on the graphology node attribute copy-through", () => {
    const graph = buildMemoryGraph(
      makePayload({
        nodes: [
          makeNode({ id: "e", note_type: "entity" }),
          makeNode({ id: "c", note_type: "claim" }),
        ],
      }),
    );
    expect(graph.getNodeAttributes("e").note_type).toBe("entity");
    expect(graph.getNodeAttributes("c").note_type).toBe("claim");
  });

  it("overrides enrichment note types to ORPHAN_OVERRIDE when is_orphan is true", () => {
    const graph = buildMemoryGraph(
      makePayload({
        nodes: [
          makeNode({ id: "e", note_type: "entity", is_orphan: true }),
          makeNode({ id: "c", note_type: "claim", is_orphan: true }),
        ],
      }),
    );
    expect(graph.getNodeAttributes("e").color).toBe(ORPHAN_OVERRIDE);
    expect(graph.getNodeAttributes("c").color).toBe(ORPHAN_OVERRIDE);
  });
});

// ── Size floor / ceiling ────────────────────────────────────────────────────

describe("buildMemoryGraph — size scaling", () => {
  it("produces size ≥ MIN_NODE_SIZE for connection_count = 0", () => {
    const graph = buildMemoryGraph(
      makePayload({ nodes: [makeNode({ id: "z", connection_count: 0 })] }),
    );
    const size = graph.getNodeAttributes("z").size;
    expect(size).toBeGreaterThanOrEqual(MIN_NODE_SIZE);
  });

  it("produces size = MIN_NODE_SIZE exactly for connection_count = 0 (floor)", () => {
    expect(sizeForConnectionCount(0)).toBe(MIN_NODE_SIZE);
  });

  it("caps size at MAX_NODE_SIZE for very high connection_count", () => {
    const graph = buildMemoryGraph(
      makePayload({ nodes: [makeNode({ id: "hub", connection_count: 1000 })] }),
    );
    expect(graph.getNodeAttributes("hub").size).toBeLessThanOrEqual(MAX_NODE_SIZE);
    expect(sizeForConnectionCount(1000)).toBe(MAX_NODE_SIZE);
    expect(sizeForConnectionCount(100)).toBe(MAX_NODE_SIZE);
  });

  it("scales size monotonically with connection_count within the band", () => {
    const s0 = sizeForConnectionCount(0);
    const s4 = sizeForConnectionCount(4);
    const s16 = sizeForConnectionCount(16);
    expect(s0).toBeLessThan(s4);
    expect(s4).toBeLessThan(s16);
  });
});

// ── Edge kind classification ────────────────────────────────────────────────

describe("buildMemoryGraph — edge kind classification", () => {
  it("classifies resolved edges as wikilink", () => {
    const graph = buildMemoryGraph(
      makePayload({
        nodes: [makeNode({ id: "a" }), makeNode({ id: "b" })],
        edges: [{ source_id: "a", target_id: "b", raw_text: "[[b]]" }],
      }),
    );
    const edgeAttrs = graph.getEdgeAttributes(graph.edges()[0]);
    expect(edgeAttrs.kind).toBe("wikilink");
    expect(edgeAttrs.raw_text).toBe("[[b]]");
  });

  it("produces a broken stub edge for each broken_targets entry", () => {
    const graph = buildMemoryGraph(
      makePayload({
        nodes: [
          makeNode({ id: "a", broken_targets: ["[[Missing Note]]", "[[Also Missing]]"] }),
        ],
        edges: [],
      }),
    );
    const brokenEdges = graph.filterEdges((_, attrs) => attrs.kind === "broken");
    expect(brokenEdges).toHaveLength(2);
    const rawTexts = brokenEdges.map((e) => graph.getEdgeAttributes(e).raw_text);
    expect(rawTexts).toContain("[[Missing Note]]");
    expect(rawTexts).toContain("[[Also Missing]]");
    // All broken edges are self-loops on the source node.
    for (const e of brokenEdges) {
      expect(graph.isUndirected(e) || graph.isDirected(e)).toBe(true);
      const [src, tgt] = graph.extremities(e);
      expect(src).toBe("a");
      expect(tgt).toBe("a");
    }
  });

  it("produces exactly one broken edge with the raw text as label", () => {
    const graph = buildMemoryGraph(
      makePayload({
        nodes: [makeNode({ id: "a", broken_targets: ["X"] })],
      }),
    );
    const brokenEdges = graph.filterEdges((_, attrs) => attrs.kind === "broken");
    expect(brokenEdges).toHaveLength(1);
    expect(graph.getEdgeAttributes(brokenEdges[0]).raw_text).toBe("X");
  });

  it("produces no broken edges when broken_targets is empty", () => {
    const graph = buildMemoryGraph(
      makePayload({
        nodes: [makeNode({ id: "a", broken_targets: [] })],
      }),
    );
    const brokenEdges = graph.filterEdges((_, attrs) => attrs.kind === "broken");
    expect(brokenEdges).toHaveLength(0);
  });

  it("silently drops edges whose source_id is unknown", () => {
    const graph = buildMemoryGraph(
      makePayload({
        nodes: [makeNode({ id: "a" })],
        edges: [{ source_id: "a", target_id: "ghost", raw_text: "[[ghost]]" }],
      }),
    );
    expect(graph.size).toBe(0);
  });

  it("silently drops edges whose target_id is unknown", () => {
    const graph = buildMemoryGraph(
      makePayload({
        nodes: [makeNode({ id: "a" })],
        edges: [{ source_id: "ghost", target_id: "a", raw_text: "[[ghost]]" }],
      }),
    );
    expect(graph.size).toBe(0);
  });
});

// ── Node attribute copy-through ─────────────────────────────────────────────

describe("buildMemoryGraph — node attribute copy-through", () => {
  it("copies permalink, note_type, is_orphan, broken_targets onto node attrs", () => {
    const graph = buildMemoryGraph(
      makePayload({
        nodes: [
          makeNode({
            id: "a",
            permalink: "design/foo",
            note_type: "pitfall",
            is_orphan: true,
            broken_targets: ["[[x]]"],
          }),
        ],
      }),
    );
    const attrs = graph.getNodeAttributes("a");
    expect(attrs.permalink).toBe("design/foo");
    expect(attrs.note_type).toBe("pitfall");
    expect(attrs.is_orphan).toBe(true);
    expect(attrs.broken_targets).toEqual(["[[x]]"]);
    expect(attrs.label).toBe("Test Note");
  });
});

// ── Empty / robustness ──────────────────────────────────────────────────────

describe("buildMemoryGraph — empty and robustness", () => {
  it("produces an empty graph for empty nodes", () => {
    const graph = buildMemoryGraph(makePayload());
    expect(graph.order).toBe(0);
    expect(graph.size).toBe(0);
  });

  it("handles missing note_type gracefully (default color)", () => {
    // Simulate a node missing note_type by passing an empty string.
    const graph = buildMemoryGraph(
      makePayload({
        nodes: [
          {
            id: "a",
            title: "A",
            permalink: "p/a",
            folder: "p",
            note_type: "",
            connection_count: 0,
          },
        ],
      }),
    );
    expect(graph.getNodeAttributes("a").color).toBe(DEFAULT_COLOR);
  });

  it("does not throw on negative connection_count (treats as 0)", () => {
    const graph = buildMemoryGraph(
      makePayload({ nodes: [makeNode({ id: "a", connection_count: -5 })] }),
    );
    expect(graph.getNodeAttributes("a").size).toBe(MIN_NODE_SIZE);
  });
});

// ── parseMemoryGraphResponse ────────────────────────────────────────────────

describe("parseMemoryGraphResponse", () => {
  it("returns null for an error response", () => {
    expect(parseMemoryGraphResponse({ error: "boom" })).toBeNull();
  });

  it("returns null for a non-object", () => {
    expect(parseMemoryGraphResponse(null)).toBeNull();
    expect(parseMemoryGraphResponse("hello")).toBeNull();
    expect(parseMemoryGraphResponse(42)).toBeNull();
  });

  it("returns null when nodes is missing or not an array", () => {
    expect(parseMemoryGraphResponse({})).toBeNull();
    expect(parseMemoryGraphResponse({ nodes: "not-array" })).toBeNull();
  });

  it("parses a well-formed response", () => {
    const parsed = parseMemoryGraphResponse({
      nodes: [
        {
          id: "a",
          permalink: "p/a",
          title: "A",
          note_type: "adr",
          folder: "p",
          connection_count: 3,
          is_orphan: false,
          broken_targets: ["[[x]]"],
        },
      ],
      edges: [{ source_id: "a", target_id: "a-self", raw_text: "[[x]]" }],
    });
    expect(parsed).not.toBeNull();
    expect(parsed!.nodes).toHaveLength(1);
    expect(parsed!.nodes[0].id).toBe("a");
    expect(parsed!.nodes[0].broken_targets).toEqual(["[[x]]"]);
    expect(parsed!.edges).toHaveLength(1);
  });

  it("coerces missing fields to defaults", () => {
    const parsed = parseMemoryGraphResponse({ nodes: [{ id: "a" }] });
    expect(parsed!.nodes[0].note_type).toBe("");
    expect(parsed!.nodes[0].connection_count).toBe(0);
    expect(parsed!.nodes[0].is_orphan).toBe(false);
    expect(parsed!.nodes[0].broken_targets).toEqual([]);
  });

  it("filters out nodes with empty ids", () => {
    const parsed = parseMemoryGraphResponse({ nodes: [{ id: "" }, { id: "a" }] });
    expect(parsed!.nodes).toHaveLength(1);
  });

  it("treats missing edges as an empty array", () => {
    const parsed = parseMemoryGraphResponse({ nodes: [{ id: "a" }] });
    expect(parsed!.edges).toEqual([]);
  });
});

// ── Seeded PRNG ─────────────────────────────────────────────────────────────

describe("createSeededRandom", () => {
  it("produces a deterministic stream for the same seed", () => {
    const rng1 = createSeededRandom(123);
    const rng2 = createSeededRandom(123);
    const seq1 = Array.from({ length: 5 }, () => rng1());
    const seq2 = Array.from({ length: 5 }, () => rng2());
    expect(seq1).toEqual(seq2);
  });

  it("produces values in [0, 1)", () => {
    const rng = createSeededRandom(1);
    for (let i = 0; i < 100; i++) {
      const v = rng();
      expect(v).toBeGreaterThanOrEqual(0);
      expect(v).toBeLessThan(1);
    }
  });

  it("produces different streams for different seeds", () => {
    const rng1 = createSeededRandom(1);
    const rng2 = createSeededRandom(2);
    const seq1 = Array.from({ length: 5 }, () => rng1());
    const seq2 = Array.from({ length: 5 }, () => rng2());
    expect(seq1).not.toEqual(seq2);
  });
});

// ── Community metadata wiring (ewfa epic integration) ───────────────────────

describe("buildClusteredMemoryGraph — community metadata wiring", () => {
  it("writes communityId and communityLabel onto clustered nodes", () => {
    const payload = makePayload({
      nodes: [
        { id: "a", title: "Agent Runtime Design", permalink: "design/a", folder: "design", note_type: "design", connection_count: 2 },
        { id: "b", title: "Agent Runtime Hooks", permalink: "design/b", folder: "design", note_type: "design", connection_count: 2 },
        { id: "c", title: "Worker Runtime Roadmap", permalink: "design/c", folder: "design", note_type: "design", connection_count: 2 },
        { id: "d", title: "Memory Retrieval Design", permalink: "design/d", folder: "design", note_type: "design", connection_count: 2 },
        { id: "e", title: "Memory Retrieval Ranking", permalink: "design/e", folder: "design", note_type: "design", connection_count: 2 },
        { id: "f", title: "Context Retrieval Notes", permalink: "design/f", folder: "design", note_type: "design", connection_count: 2 },
      ],
      edges: [
        { source_id: "a", target_id: "b", raw_text: "[[Agent Runtime Hooks]]" },
        { source_id: "b", target_id: "c", raw_text: "[[Worker Runtime Roadmap]]" },
        { source_id: "a", target_id: "c", raw_text: "[[Worker Runtime Roadmap]]" },
        { source_id: "d", target_id: "e", raw_text: "[[Memory Retrieval Ranking]]" },
        { source_id: "e", target_id: "f", raw_text: "[[Context Retrieval Notes]]" },
        { source_id: "d", target_id: "f", raw_text: "[[Context Retrieval Notes]]" },
        { source_id: "c", target_id: "d", raw_text: "[[Memory Retrieval Design]]" },
      ],
    });

    const { graph, communities } = buildClusteredMemoryGraph(payload);

    expect(communities.size).toBeGreaterThan(0);

    // Every clustered node carries both community attributes.
    for (const [nodeId, metadata] of communities) {
      expect(graph.getNodeAttribute(nodeId, MEMORY_COMMUNITY_ID_ATTRIBUTE)).toBe(
        metadata.communityId,
      );
      expect(graph.getNodeAttribute(nodeId, MEMORY_COMMUNITY_LABEL_ATTRIBUTE)).toBe(
        metadata.label,
      );
      expect(metadata.communityId).toMatch(/^[0-9a-f]{16}$/);
    }

    // Community ids are stable across a second build of the same payload.
    const second = buildClusteredMemoryGraph(payload);
    const firstIds = [...new Set([...communities.values()].map((c) => c.communityId))].sort();
    const secondIds = [...new Set([...second.communities.values()].map((c) => c.communityId))].sort();
    expect(secondIds).toEqual(firstIds);
  });

  it("leaves singleton/unclustered nodes without community attributes", () => {
    const payload = makePayload({
      nodes: [
        { id: "hub", title: "Hub Note", permalink: "p/hub", folder: "p", note_type: "design", connection_count: 1 },
        { id: "hub-friend", title: "Hub Friend", permalink: "p/hub-friend", folder: "p", note_type: "design", connection_count: 1 },
        { id: "lonely", title: "Lonely Note", permalink: "p/lonely", folder: "p", note_type: "design", connection_count: 0 },
      ],
      edges: [
        { source_id: "hub", target_id: "hub-friend", raw_text: "[[Hub Friend]]" },
      ],
    });

    const { graph, communities } = buildClusteredMemoryGraph(payload);

    // The connected pair clusters; the isolated singleton does not.
    expect(communities.has("hub")).toBe(true);
    expect(communities.has("hub-friend")).toBe(true);
    expect(communities.has("lonely")).toBe(false);

    // Clustered nodes carry the attributes.
    expect(graph.getNodeAttribute("hub", MEMORY_COMMUNITY_ID_ATTRIBUTE)).toBeDefined();
    expect(graph.getNodeAttribute("hub-friend", MEMORY_COMMUNITY_ID_ATTRIBUTE)).toBeDefined();

    // The singleton has no community attributes — reducers can detect
    // absence and keep default styling, and the attraction pass skips it.
    expect(graph.getNodeAttribute("lonely", MEMORY_COMMUNITY_ID_ATTRIBUTE)).toBeUndefined();
    expect(graph.getNodeAttribute("lonely", MEMORY_COMMUNITY_LABEL_ATTRIBUTE)).toBeUndefined();
  });
});

describe("per-community attraction no-op guarantees", () => {
  it("is a no-op for a single-node graph (no communities)", () => {
    const payload = makePayload({
      nodes: [
        { id: "lonely", title: "Standalone Note", permalink: "p/lonely", folder: "p", note_type: "design", connection_count: 0 },
      ],
      edges: [],
    });

    const { graph, communities } = buildClusteredMemoryGraph(payload);

    // No clustered communities at all.
    expect(communities.size).toBe(0);

    const beforeX = graph.getNodeAttribute("lonely", "x");
    const beforeY = graph.getNodeAttribute("lonely", "y");

    const result = applyPerCommunityAttraction(graph, communities, {
      clusterRadius: 400,
      strength: 0.1,
    });

    expect(result.orderedCommunityIds).toEqual([]);
    expect(result.eligibleNodeCount).toBe(0);
    expect(result.movedNodeCount).toBe(0);
    expect(graph.getNodeAttribute("lonely", "x")).toBe(beforeX);
    expect(graph.getNodeAttribute("lonely", "y")).toBe(beforeY);
  });

  it("is a no-op when no node carries community metadata", () => {
    // A graph with nodes but no clustering applied — mirrors the unclustered
    // state. `applyMemoryCommunities` on an edgeless graph returns an empty map.
    const graph = buildMemoryGraphFromPayload(
      makePayload({
        nodes: [
          { id: "a", title: "Isolated A", permalink: "p/a", folder: "p", note_type: "design", connection_count: 0 },
          { id: "b", title: "Isolated B", permalink: "p/b", folder: "p", note_type: "design", connection_count: 0 },
        ],
        edges: [],
      }),
    );

    const communities = applyMemoryCommunities(graph);
    expect(communities.size).toBe(0);

    const positionsBefore = new Map<string, { x: number; y: number }>();
    graph.forEachNode((id, attrs) => {
      positionsBefore.set(id, { x: attrs.x, y: attrs.y });
    });

    const result = applyPerCommunityAttraction(graph, communities);
    expect(result.movedNodeCount).toBe(0);

    graph.forEachNode((id, attrs) => {
      const before = positionsBefore.get(id)!;
      expect(attrs.x).toBe(before.x);
      expect(attrs.y).toBe(before.y);
    });
  });
});

describe("postLayout composition (FA2 → attraction ordering)", () => {
  it("moving clustered nodes toward deterministic centers leaves unclustered nodes untouched", () => {
    // Simulate the post-FA2 state: clustered nodes have settled positions,
    // an unclustered node sits somewhere else. The attraction pass must pull
    // only the clustered nodes toward their community center.
    const payload = makePayload({
      nodes: [
        { id: "a", title: "Alpha Cluster", permalink: "p/a", folder: "p", note_type: "design", connection_count: 2 },
        { id: "b", title: "Alpha Cluster", permalink: "p/b", folder: "p", note_type: "design", connection_count: 2 },
        { id: "c", title: "Beta Cluster", permalink: "p/c", folder: "p", note_type: "design", connection_count: 2 },
        { id: "d", title: "Beta Cluster", permalink: "p/d", folder: "p", note_type: "design", connection_count: 2 },
        { id: "lonely", title: "Lonely", permalink: "p/lonely", folder: "p", note_type: "design", connection_count: 0 },
      ],
      edges: [
        { source_id: "a", target_id: "b", raw_text: "[[Alpha Cluster]]" },
        { source_id: "c", target_id: "d", raw_text: "[[Beta Cluster]]" },
      ],
    });

    const { graph, communities } = buildClusteredMemoryGraph(payload);

    // Scatter the clustered nodes far from origin to simulate post-FA2 spread.
    graph.setNodeAttribute("a", "x", 500);
    graph.setNodeAttribute("a", "y", 500);
    graph.setNodeAttribute("b", "x", -500);
    graph.setNodeAttribute("b", "y", -500);
    graph.setNodeAttribute("c", "x", 300);
    graph.setNodeAttribute("c", "y", -300);
    graph.setNodeAttribute("d", "x", -300);
    graph.setNodeAttribute("d", "y", 300);
    const lonelyX = 42;
    const lonelyY = -17;
    graph.setNodeAttribute("lonely", "x", lonelyX);
    graph.setNodeAttribute("lonely", "y", lonelyY);

    const result = applyPerCommunityAttraction(graph, communities, {
      clusterRadius: 50,
      strength: 0.5,
      iterations: 10,
    });

    // Two distinct communities were detected.
    expect(result.orderedCommunityIds.length).toBe(2);
    expect(result.eligibleNodeCount).toBe(4);
    expect(result.movedNodeCount).toBe(4);

    // The unclustered node is untouched — it never had community metadata.
    expect(graph.getNodeAttribute("lonely", "x")).toBe(lonelyX);
    expect(graph.getNodeAttribute("lonely", "y")).toBe(lonelyY);

    // Clustered nodes moved toward their assigned centers (distance shrunk).
    for (const nodeId of ["a", "b", "c", "d"]) {
      const communityId = graph.getNodeAttribute(nodeId, MEMORY_COMMUNITY_ID_ATTRIBUTE) as string;
      const center = result.centers.get(communityId);
      expect(center).toBeDefined();
      const x = graph.getNodeAttribute(nodeId, "x") as number;
      const y = graph.getNodeAttribute(nodeId, "y") as number;
      // After multiple iterations at strength 0.5 the nodes should be
      // measurably closer to their center than their starting 300-500 spread.
      const dist = Math.hypot(center!.x - x, center!.y - y);
      expect(dist).toBeLessThan(100);
    }
  });
});

describe("empty payload handling", () => {
  it("buildMemoryGraphFromPayload handles an empty node list", () => {
    const graph = buildMemoryGraphFromPayload(makePayload());
    expect(graph.order).toBe(0);
    expect(graph.size).toBe(0);
  });

  it("drops edges referencing unknown nodes", () => {
    const graph = buildMemoryGraphFromPayload(
      makePayload({
        nodes: [
          { id: "a", title: "A", permalink: "p/a", folder: "p", note_type: "design", connection_count: 0 },
        ],
        edges: [
          { source_id: "a", target_id: "ghost", raw_text: "[[ghost]]" },
        ],
      }),
    );
    expect(graph.order).toBe(1);
    expect(graph.size).toBe(0);
  });
});
