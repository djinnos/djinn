/**
 * memoryGraphAdapter — shared pure helpers for the memory graph canvas:
 * the deterministic `note_type → color` palette, typed-edge styling, a
 * seeded PRNG, and the defensive `memory_graph` response parser.
 *
 * Pure functions only — no React, no fetch, no canvas. The rendering
 * component (`MemoryGraphCanvas`) wires those in.
 */

import type { MemoryGraphOutput } from "@/api/generated/mcp-tools.gen";

// ── Color palette ───────────────────────────────────────────────────────────

/**
 * Deterministic `note_type → color` palette. Each known type maps to a
 * Tailwind-400 hue that reads on the near-black canvas. Unknown types fall
 * back to `DEFAULT_COLOR`. Orphan nodes are overridden to `ORPHAN_OVERRIDE`
 * regardless of their type.
 *
 * The enrichment-created note types (`entity`, `claim`) — see epic diei /
 * task qp5s — get distinct hues so they read as a separate class of node:
 *   - `entity`: teal-400 — recurring systems / concepts surfaced by the
 *     enrichment pass.
 *   - `claim`: pink-400 — the decisions the memory records.
 */
export const PALETTE: Readonly<Record<string, string>> = Object.freeze({
  adr: "#a78bfa", // violet-400
  pattern: "#60a5fa", // blue-400
  case: "#34d399", // emerald-400
  pitfall: "#f87171", // red-400
  research: "#fbbf24", // amber-400
  reference: "#94a3b8", // slate-400
  entity: "#2dd4bf", // teal-400 — enrichment-created recurring system
  claim: "#f472b6", // pink-400 — enrichment-created decision
});

/** Fallback color for nodes whose `note_type` is not in the palette. */
export const DEFAULT_COLOR = "#cbd5e1"; // slate-300

/**
 * Orphan override — a node with `is_orphan: true` is always red regardless
 * of its `note_type`. This is the visual flag the epic calls for.
 */
export const ORPHAN_OVERRIDE = "#ef4444"; // red-500

/**
 * Resolve the display color for a node. Orphan nodes always win (red);
 * otherwise the palette maps `note_type` to a hue, with `DEFAULT_COLOR` as
 * the fallback for unknown types.
 */
export function colorForNote(noteType: string, isOrphan: boolean): string {
  if (isOrphan) return ORPHAN_OVERRIDE;
  return PALETTE[noteType] ?? DEFAULT_COLOR;
}

// ── Seeded PRNG ─────────────────────────────────────────────────────────────

/**
 * Small deterministic PRNG (mulberry32) so the same seed always produces the
 * same stream (starfield positions, layout jitter). Exported for tests that
 * want to assert the exact stream.
 */
export function createSeededRandom(seed: number): () => number {
  let state = seed >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let t = state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

// ── Edge styling ────────────────────────────────────────────────────────────

/** Typed semantic edge kinds surfaced from `note_associations`. */
export type TypedEdgeKind = "builds_on" | "contradicts" | "supersedes" | "exemplifies" | "derived_from";

/**
 * Per-kind styling for typed association edges surfaced from `note_associations`.
 * The canvas reads these to render distinct visual lanes.
 */
export const TYPED_EDGE_STYLES: Readonly<Record<string, { color: string; size: number; dashed: boolean }>> = Object.freeze({
  supersedes:   { color: "#22c55e", size: 2.0, dashed: false }, // green-500
  contradicts:  { color: "#ef4444", size: 2.0, dashed: true },   // red-500
  builds_on:    { color: "#3b82f6", size: 1.5, dashed: false }, // blue-500
  exemplifies:  { color: "#f59e0b", size: 1.5, dashed: false }, // amber-500
  derived_from: { color: "#8b5cf6", size: 1.5, dashed: false }, // violet-500
});

/** Set of all typed edge kinds for fast membership tests. */
export const TYPED_EDGE_KINDS = new Set<string>(["builds_on", "contradicts", "supersedes", "exemplifies", "derived_from"]);

// ── Response parser (pure runtime guard) ────────────────────────────────────

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

const NOTE_LIFECYCLE_STATUSES = new Set(["active", "archived", "deprecated"]);

/**
 * A wire timestamp must be a real ISO-8601 date-time, not merely any string.
 *
 * `Date.parse` and the `Date` constructor silently normalize overflowed
 * calendar components rather than rejecting them — e.g. `2024-02-30T00:00:00Z`
 * is accepted as March 1 by most ECMAScript engines. To reject impossible
 * calendar dates (including a non-leap-year Feb 29), we reconstruct the
 * instant from the explicit captured components via `Date.UTC`, then
 * round-trip it back through a `Date` and confirm the year/month/day/
 * hour/minute/second survived unchanged. A numeric trailing timezone offset
 * must also have valid hour/minute components before it can be retained.
 */
function isIsoTimestamp(value: unknown): value is string {
  if (typeof value !== "string") return false;
  const m =
    /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})(?:\.\d+)?(Z|[+-]\d{2}:\d{2})$/.exec(value);
  if (m === null) return false;
  const year = Number(m[1]);
  const month = Number(m[2]);
  const day = Number(m[3]);
  const hour = Number(m[4]);
  const minute = Number(m[5]);
  const second = Number(m[6]);
  const timezone = m[7];
  if (timezone !== "Z") {
    const offsetHour = Number(timezone.slice(1, 3));
    const offsetMinute = Number(timezone.slice(4, 6));
    if (offsetHour > 23 || offsetMinute > 59) return false;
  }
  const epoch = Date.UTC(year, month - 1, day, hour, minute, second);
  if (!Number.isFinite(epoch)) return false;
  const d = new Date(epoch);
  return (
    d.getUTCFullYear() === year &&
    d.getUTCMonth() === month - 1 &&
    d.getUTCDate() === day &&
    d.getUTCHours() === hour &&
    d.getUTCMinutes() === minute &&
    d.getUTCSeconds() === second
  );
}

function isNonNegativeInteger(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value) && Number.isInteger(value) && value >= 0;
}

/**
 * Parse the bounded inactive-node counters only as a complete, trustworthy
 * unit. An absent or malformed summary remains absent at the UI boundary.
 */
function parseLifecycleSummary(
  value: unknown,
): NonNullable<MemoryGraphOutput["lifecycle_summary"]> | undefined {
  if (!isRecord(value)) return undefined;
  const { inactive_omitted, inactive_returned, inactive_total } = value;
  if (
    !isNonNegativeInteger(inactive_omitted) ||
    !isNonNegativeInteger(inactive_returned) ||
    !isNonNegativeInteger(inactive_total)
  ) {
    return undefined;
  }
  return { inactive_omitted, inactive_returned, inactive_total };
}

/**
 * Parse a raw `memory_graph` MCP response into a typed `MemoryGraphOutput`,
 * or return `null` when the response is malformed / errored.
 *
 * Mirrors the defensive style of `parseSnapshotResponse` in
 * `codeGraphAdapter.ts`: returns `null` on an `error` field, missing
 * `nodes`, or wrong types, so the caller can render the empty state.
 */
export function parseMemoryGraphResponse(raw: unknown): MemoryGraphOutput | null {
  if (!isRecord(raw)) return null;
  if (typeof raw.error === "string" && raw.error.length > 0) return null;
  if (!Array.isArray(raw.nodes)) return null;

  const nodes = raw.nodes.filter(isRecord) as Array<Record<string, unknown>>;
  const edges = Array.isArray(raw.edges)
    ? (raw.edges.filter(isRecord) as Array<Record<string, unknown>>)
    : [];

  const parsedNodes = nodes
    .map((n) => {
      const brokenTargets = Array.isArray(n.broken_targets)
        ? (n.broken_targets.filter((t): t is string => typeof t === "string"))
        : [];
      return {
        id: String(n.id ?? ""),
        permalink: typeof n.permalink === "string" ? n.permalink : "",
        title: typeof n.title === "string" ? n.title : "",
        note_type: typeof n.note_type === "string" ? n.note_type : "",
        folder: typeof n.folder === "string" ? n.folder : "",
        connection_count:
          typeof n.connection_count === "number" && Number.isFinite(n.connection_count)
            ? n.connection_count
            : 0,
        is_orphan: n.is_orphan === true,
        broken_targets: brokenTargets,
        // Optional wire fields the graph canvas reads: `entity_type` (note vs
        // proposal glyph) and `created_at`. The wire type is an ISO-8601
        // string; numeric epoch seconds (accepted for fixtures) normalize to
        // ISO so the parsed shape always matches `GraphNode`.
        ...(typeof n.entity_type === "string" ? { entity_type: n.entity_type } : {}),
        ...(typeof n.created_at === "string"
          ? { created_at: n.created_at }
          : typeof n.created_at === "number" && Number.isFinite(n.created_at)
            ? { created_at: new Date(n.created_at * 1000).toISOString() }
            : {}),
        // Lifecycle fields are optional for the legacy active-only response.
        // Never synthesize transition times: explicit null and omission stay
        // omitted, while only the server's exact status vocabulary is kept.
        ...(typeof n.status === "string" && NOTE_LIFECYCLE_STATUSES.has(n.status)
          ? { status: n.status }
          : {}),
        ...(isIsoTimestamp(n.lifecycle_changed_at)
          ? { lifecycle_changed_at: n.lifecycle_changed_at }
          : {}),
      };
    })
    .filter((n) => n.id.length > 0);

  const parsedEdges = edges
    .map((e) => ({
      source_id: String(e.source_id ?? ""),
      target_id: String(e.target_id ?? ""),
      raw_text: typeof e.raw_text === "string" ? e.raw_text : "",
    }))
    .filter((e) => e.source_id.length > 0 && e.target_id.length > 0);

  const typedEdges = Array.isArray(raw.typed_edges)
    ? (raw.typed_edges.filter(isRecord) as Array<Record<string, unknown>>)
    : [];
  const parsedTypedEdges = typedEdges
    .map((e) => ({
      source_id: String(e.source_id ?? ""),
      target_id: String(e.target_id ?? ""),
      kind: typeof e.kind === "string" ? e.kind : "",
      weight: typeof e.weight === "number" && Number.isFinite(e.weight) ? e.weight : 0,
    }))
    .filter((e) => e.source_id.length > 0 && e.target_id.length > 0 && e.kind.length > 0);

  const lifecycleSummary = parseLifecycleSummary(raw.lifecycle_summary);

  return {
    nodes: parsedNodes,
    edges: parsedEdges,
    typed_edges: parsedTypedEdges,
    ...(lifecycleSummary !== undefined ? { lifecycle_summary: lifecycleSummary } : {}),
  };
}
