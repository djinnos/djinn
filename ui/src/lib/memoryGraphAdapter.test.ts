/**
 * memoryGraphAdapter tests — the pure helper surface consumed by
 * `MemoryGraphCanvas`: the `note_type → color` palette, the seeded PRNG,
 * typed-edge styling, and the defensive `memory_graph` response parser
 * (including the optional `created_at` / `entity_type` passthrough that
 * drives the canvas time axis and proposal glyphs).
 */

import { describe, expect, it } from "vitest";

import {
  colorForNote,
  createSeededRandom,
  DEFAULT_COLOR,
  ORPHAN_OVERRIDE,
  PALETTE,
  parseMemoryGraphResponse,
  TYPED_EDGE_KINDS,
  TYPED_EDGE_STYLES,
} from "@/lib/memoryGraphAdapter";

// ── colorForNote / palette ───────────────────────────────────────────────────

describe("colorForNote", () => {
  it("maps each known note_type to its palette color", () => {
    for (const [noteType, color] of Object.entries(PALETTE)) {
      expect(colorForNote(noteType, false)).toBe(color);
    }
  });

  it("falls back to DEFAULT_COLOR for unknown and empty note types", () => {
    expect(colorForNote("mystery", false)).toBe(DEFAULT_COLOR);
    expect(colorForNote("", false)).toBe(DEFAULT_COLOR);
  });

  it("overrides to ORPHAN_OVERRIDE when is_orphan is true, regardless of note_type", () => {
    expect(colorForNote("adr", true)).toBe(ORPHAN_OVERRIDE);
    expect(colorForNote("entity", true)).toBe(ORPHAN_OVERRIDE);
    expect(colorForNote("mystery", true)).toBe(ORPHAN_OVERRIDE);
  });

  it("gives enrichment entity/claim types hues distinct from every other palette color", () => {
    const others = Object.entries(PALETTE)
      .filter(([k]) => k !== "entity" && k !== "claim")
      .map(([, v]) => v);
    expect(others).not.toContain(PALETTE.entity);
    expect(others).not.toContain(PALETTE.claim);
    expect(PALETTE.entity).not.toBe(PALETTE.claim);
  });
});

// ── Typed edge styling ───────────────────────────────────────────────────────

describe("TYPED_EDGE_STYLES", () => {
  it("defines a style for every typed edge kind", () => {
    for (const kind of TYPED_EDGE_KINDS) {
      expect(TYPED_EDGE_STYLES[kind]).toBeDefined();
      expect(TYPED_EDGE_STYLES[kind].color).toMatch(/^#[0-9a-fA-F]{6}$/);
    }
  });

  it("renders contradicts as the only dashed lane", () => {
    const dashed = Object.entries(TYPED_EDGE_STYLES)
      .filter(([, s]) => s.dashed)
      .map(([k]) => k);
    expect(dashed).toEqual(["contradicts"]);
  });
});

// ── createSeededRandom ───────────────────────────────────────────────────────

describe("createSeededRandom", () => {
  it("produces a deterministic stream for the same seed", () => {
    const a = createSeededRandom(42);
    const b = createSeededRandom(42);
    for (let i = 0; i < 32; i += 1) {
      expect(a()).toBe(b());
    }
  });

  it("produces values in [0, 1)", () => {
    const rng = createSeededRandom(7);
    for (let i = 0; i < 256; i += 1) {
      const v = rng();
      expect(v).toBeGreaterThanOrEqual(0);
      expect(v).toBeLessThan(1);
    }
  });

  it("produces different streams for different seeds", () => {
    const a = createSeededRandom(1);
    const b = createSeededRandom(2);
    const streamA = Array.from({ length: 8 }, () => a());
    const streamB = Array.from({ length: 8 }, () => b());
    expect(streamA).not.toEqual(streamB);
  });
});

// ── parseMemoryGraphResponse ─────────────────────────────────────────────────

const wellFormed = {
  nodes: [
    {
      id: "a",
      permalink: "memory/adr/a",
      title: "Note A",
      note_type: "adr",
      folder: "memory/adr",
      connection_count: 3,
      is_orphan: false,
      broken_targets: ["ghost"],
    },
    {
      id: "b",
      permalink: "memory/case/b",
      title: "Note B",
      note_type: "case",
      folder: "memory/case",
      connection_count: 1,
      is_orphan: true,
      broken_targets: [],
    },
  ],
  edges: [{ source_id: "a", target_id: "b", raw_text: "Note B" }],
  typed_edges: [{ source_id: "b", target_id: "a", kind: "builds_on", weight: 0.8 }],
};

describe("parseMemoryGraphResponse", () => {
  it("returns null for an error response", () => {
    expect(parseMemoryGraphResponse({ error: "boom", nodes: [] })).toBeNull();
  });

  it("returns null for a non-object", () => {
    expect(parseMemoryGraphResponse(null)).toBeNull();
    expect(parseMemoryGraphResponse("nope")).toBeNull();
    expect(parseMemoryGraphResponse([1, 2])).toBeNull();
  });

  it("returns null when nodes is missing or not an array", () => {
    expect(parseMemoryGraphResponse({})).toBeNull();
    expect(parseMemoryGraphResponse({ nodes: "x" })).toBeNull();
  });

  it("parses a well-formed response", () => {
    const parsed = parseMemoryGraphResponse(wellFormed);
    expect(parsed).not.toBeNull();
    expect(parsed!.nodes).toHaveLength(2);
    expect(parsed!.nodes[0]).toMatchObject({
      id: "a",
      note_type: "adr",
      connection_count: 3,
      broken_targets: ["ghost"],
    });
    expect(parsed!.edges).toEqual([{ source_id: "a", target_id: "b", raw_text: "Note B" }]);
    expect(parsed!.typed_edges).toEqual([
      { source_id: "b", target_id: "a", kind: "builds_on", weight: 0.8 },
    ]);
  });

  it("coerces missing fields to defaults", () => {
    const parsed = parseMemoryGraphResponse({ nodes: [{ id: "x" }] });
    expect(parsed!.nodes[0]).toMatchObject({
      id: "x",
      permalink: "",
      title: "",
      note_type: "",
      folder: "",
      connection_count: 0,
      is_orphan: false,
      broken_targets: [],
    });
  });

  it("filters out nodes with empty ids", () => {
    const parsed = parseMemoryGraphResponse({ nodes: [{ id: "" }, { id: "ok" }] });
    expect(parsed!.nodes.map((n) => n.id)).toEqual(["ok"]);
  });

  it("treats missing edges as an empty array", () => {
    const parsed = parseMemoryGraphResponse({ nodes: [{ id: "x" }] });
    expect(parsed!.edges).toEqual([]);
    expect(parsed!.typed_edges).toEqual([]);
  });

  it("drops typed edges with missing endpoints or kind", () => {
    const parsed = parseMemoryGraphResponse({
      nodes: [{ id: "x" }],
      typed_edges: [
        { source_id: "x", target_id: "", kind: "builds_on" },
        { source_id: "x", target_id: "y", kind: "" },
        { source_id: "x", target_id: "y", kind: "supersedes" },
      ],
    });
    expect(parsed!.typed_edges).toEqual([
      { source_id: "x", target_id: "y", kind: "supersedes", weight: 0 },
    ]);
  });

  it("passes through ISO created_at and normalizes epoch seconds to ISO", () => {
    const parsed = parseMemoryGraphResponse({
      nodes: [
        { id: "num", created_at: 1773964800 },
        { id: "iso", created_at: "2026-07-12T08:00:00Z" },
      ],
    });
    expect(parsed!.nodes[0].created_at).toBe(new Date(1773964800 * 1000).toISOString());
    expect(parsed!.nodes[1].created_at).toBe("2026-07-12T08:00:00Z");
  });

  it("omits created_at when absent or non-finite", () => {
    const parsed = parseMemoryGraphResponse({
      nodes: [{ id: "none" }, { id: "nan", created_at: Number.NaN }, { id: "obj", created_at: {} }],
    });
    for (const node of parsed!.nodes) {
      expect("created_at" in node).toBe(false);
    }
  });

  it("passes through entity_type when it is a string", () => {
    const parsed = parseMemoryGraphResponse({
      nodes: [{ id: "p", entity_type: "proposal" }, { id: "n" }],
    });
    expect(parsed!.nodes[0].entity_type).toBe("proposal");
    expect("entity_type" in parsed!.nodes[1]).toBe(false);
  });
});
