/**
 * MemoryGraphCanvas — the memory knowledge graph as a radial time disk.
 *
 * A note's distance from the core is a linear map of its age — oldest at the
 * core, newest at the rim — with dated calendar rings as the axis and a
 * playable reveal that rebuilds the map chronologically. Notes are colored by
 * `note_type` (the adapter palette), sized by connection count, faded by age,
 * and fanned around the disk by a golden-angle spiral so every era spreads
 * evenly. Wikilink edges are quiet threads; typed semantic edges keep their
 * adapter styles. Rendering is plain canvas-2d.
 *
 * Timestamps: the `memory_graph` payload may carry an optional `created_at`
 * per node (epoch seconds or ISO string). When absent the layout falls back
 * to a stable ordinal spread, so undated graphs still build up in a
 * deterministic order — rings just lose their date labels.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { HugeiconsIcon } from "@hugeicons/react";
import { AlertCircleIcon, PauseIcon, PlayIcon, RefreshIcon } from "@hugeicons/core-free-icons";

import { callMcpTool } from "@/api/mcpClient";
import type { MemoryGraphInput, MemoryGraphOutput } from "@/api/generated/mcp-tools.gen";
import {
  colorForNote,
  createSeededRandom,
  parseMemoryGraphResponse,
  TYPED_EDGE_STYLES,
} from "@/lib/memoryGraphAdapter";

// ── Disk geometry ────────────────────────────────────────────────────────────

/** Empty lead-in: the oldest note sits just off the core so replay opens on a beat of emptiness. */
const LEAD_IN = 0.06;
const RING_INNER = 58;
const RING_OUTER = 340;
/** Fallback ring count for undated graphs. */
const RING_STEPS = 4;
const FIT_PADDING = 70;
const ZOOM_MIN = 0.3;
const ZOOM_MAX = 5;
/** Playback camera may zoom into the young core at most this far. */
const FIT_ZOOM_CAP = 2.2;

const recForRatio = (ratio: number) => LEAD_IN + (1 - LEAD_IN) * clamp(ratio, 0, 1);
const radiusForRecency = (rec: number) => RING_INNER + rec * (RING_OUTER - RING_INNER);

/**
 * Constant ring scale: core radius and per-ring band are pinned to the
 * canonical layout, so more history grows the disk outward instead of
 * stretching a fixed disk thinner.
 */
const RING_CORE = radiusForRecency(recForRatio(0));
const RING_BAND = (radiusForRecency(recForRatio(1)) - RING_CORE) / RING_STEPS;
const ringRadius = (i: number) => RING_CORE + i * RING_BAND;

/** Age gradient — old notes quiet, recent notes bright. */
const AGE = { mid: 0.52, midInk: 0.74, newInk: 0.95, oldInk: 0.42 };

const PLAYBACK_MS = 6000;
const DAY = 86_400;

const CANVAS_BACKGROUND = `radial-gradient(circle at 50% 45%, rgba(124, 58, 237, 0.07) 0%, transparent 65%), linear-gradient(to bottom, #06060a, #0a0a10)`;

// ── Small math helpers ───────────────────────────────────────────────────────

function clamp(v: number, lo: number, hi: number): number {
  return Math.max(lo, Math.min(hi, v));
}

/** FNV-1a — stable per-id seed for band placement jitter. */
function hash(input: string): number {
  let h = 2166136261;
  for (let i = 0; i < input.length; i += 1) {
    h ^= input.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return h >>> 0;
}

const smooth = (p: number) => p * p * (3 - 2 * p);

/** Smoothstep recency → ink alpha along the age gradient. */
function recencyInk(rec: number): number {
  const t = clamp(rec, 0, 1);
  if (t <= AGE.mid) return AGE.oldInk + (AGE.midInk - AGE.oldInk) * smooth(t / AGE.mid);
  return AGE.midInk + (AGE.newInk - AGE.midInk) * smooth((t - AGE.mid) / (1 - AGE.mid));
}

function hexToRgb(hex: string): [number, number, number] {
  let s = hex.replace("#", "");
  if (s.length === 3) s = s.split("").map((c) => c + c).join("");
  const n = Number.parseInt(s, 16);
  if (Number.isNaN(n)) return [203, 213, 225];
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}

function rgba(hex: string, alpha: number): string {
  const [r, g, b] = hexToRgb(hex);
  return `rgba(${r},${g},${b},${alpha})`;
}

// ── Disk model ───────────────────────────────────────────────────────────────

export interface DiskNode {
  id: string;
  title: string;
  permalink: string;
  noteType: string;
  isProposal: boolean;
  isOrphan: boolean;
  isGhost: boolean;
  lifecycle: "active" | "archived" | "deprecated";
  connectionCount: number;
  color: string;
  ts: number | null;
  /** Recency 0 (oldest) → 1 (newest); drives radius, ink, and reveal order. */
  rec: number;
  /** Reveal coordinate this node ignites at (staggered within its date band). */
  igniteAt: number;
  /** Time-anchored target radius (inside its ring's band). */
  tr: number;
  ring: number;
  /** Draw radius. */
  r: number;
  x: number;
  y: number;
  vx: number;
  vy: number;
}

interface DiskLink {
  a: number;
  b: number;
  kind: string;
}

interface DiskRing {
  r: number;
  label: string | null;
  /** Reveal coordinate at which this ring caps its band. */
  ratio: number;
}

export interface DiskModel {
  nodes: DiskNode[];
  links: DiskLink[];
  rings: DiskRing[];
  timed: boolean;
  minTs: number | null;
  maxTs: number | null;
}

function nodeTs(raw: Record<string, unknown>): number | null {
  const v = raw.created_at;
  if (typeof v === "number" && Number.isFinite(v)) return v;
  if (typeof v === "string") {
    const parsed = Date.parse(v);
    if (!Number.isNaN(parsed)) return Math.floor(parsed / 1000);
  }
  return null;
}

function formatDay(ts: number): string {
  return new Date(ts * 1000).toLocaleDateString(undefined, {
    day: "numeric",
    month: "short",
    timeZone: "UTC",
  });
}

// "Nice" calendar intervals, fine → coarse, with intermediate rungs so the
// bucketer can land near the target ring count.
const UNITS: Array<{ kind: "day" | "month"; step: number }> = [
  { kind: "day", step: 1 },
  { kind: "day", step: 2 },
  { kind: "day", step: 7 },
  { kind: "day", step: 14 },
  { kind: "month", step: 1 },
  { kind: "month", step: 2 },
  { kind: "month", step: 3 },
  { kind: "month", step: 6 },
  { kind: "month", step: 12 },
];

function bucketStart(ts: number, unit: (typeof UNITS)[number]): number {
  if (unit.kind === "day") {
    const period = unit.step * DAY;
    return Math.floor(ts / period) * period;
  }
  const d = new Date(ts * 1000);
  d.setUTCHours(0, 0, 0, 0);
  const absMonth = Math.floor((d.getUTCFullYear() * 12 + d.getUTCMonth()) / unit.step) * unit.step;
  d.setUTCFullYear(Math.floor(absMonth / 12), absMonth % 12, 1);
  return Math.floor(d.getTime() / 1000);
}

const populatedStarts = (stamps: number[], unit: (typeof UNITS)[number]) =>
  [...new Set(stamps.map((t) => bucketStart(t, unit)))].sort((a, b) => a - b);

/**
 * Aim for ~5–12 rings (growing ~log2 with span), snapped to a calendar
 * interval. Dense graphs push the target up so the disk grows outward and
 * thousands of notes get room instead of packing a fixed area solid.
 */
function chooseUnit(stamps: number[], spanDays: number, nodeCount: number): (typeof UNITS)[number] {
  const densityBoost = nodeCount > 200 ? Math.round(Math.log2(nodeCount / 200)) : 0;
  const target = clamp(Math.round(4 + Math.log2(Math.max(1, spanDays / 60))) + densityBoost, 5, 24);
  let best = UNITS[0];
  let bestScore = Number.POSITIVE_INFINITY;
  for (const unit of UNITS) {
    const count = populatedStarts(stamps, unit).length;
    if (!count) continue;
    const score = Math.abs(count - target) + (count > target ? 0.5 : 0);
    if (score < bestScore) {
      bestScore = score;
      best = unit;
    }
  }
  return best;
}

function bucketLabel(ts: number, unit: (typeof UNITS)[number]): string {
  if (unit.kind === "day") return formatDay(ts);
  const d = new Date(ts * 1000);
  return unit.step >= 12
    ? String(d.getUTCFullYear())
    : d.toLocaleDateString(undefined, { month: "short", timeZone: "UTC", year: "numeric" });
}

/** Place a node inside its ring's band, biased toward mid-band. */
function placeRadius(ringIndex: number, id: string): number {
  const outer = ringRadius(ringIndex);
  const inner = ringIndex > 0 ? ringRadius(ringIndex - 1) : RING_CORE - RING_BAND * 0.5;
  const h = (hash(id) % 1000) / 1000;
  return outer - (0.15 + 0.7 * h) * (outer - inner);
}

export function buildMemoryGraphDisk(payload: MemoryGraphOutput): DiskModel {
  // Missing status is the legacy active-only wire contract.
  const activeRawNodes = payload.nodes.filter((n) => n.status !== "archived" && n.status !== "deprecated");
  const ghostRawNodes = payload.nodes.filter((n) => n.status === "archived" || n.status === "deprecated");
  // With no active nodes there is no active model to preserve; retain a stable
  // fallback disk for the returned ghosts. They remain lifecycle ghosts rather
  // than silently becoming active nodes in this degenerate case.
  const usesGhostFallback = activeRawNodes.length === 0;
  const rawNodes = usesGhostFallback ? ghostRawNodes : activeRawNodes;
  const ghostsToAppend = activeRawNodes.length ? ghostRawNodes : [];
  const stamps = rawNodes.map((n) => nodeTs(n)).filter((v): v is number => v !== null);
  const minTs = stamps.length ? Math.min(...stamps) : null;
  const maxTs = stamps.length ? Math.max(...stamps) : null;
  const timed = minTs !== null && maxTs !== null && maxTs > minTs;

  // Recency: a truthful linear map of time; ordinal fallback keeps undated
  // graphs building up in a stable order.
  const ordered = [...rawNodes].sort((a, b) => {
    const at = nodeTs(a) ?? Number.POSITIVE_INFINITY;
    const bt = nodeTs(b) ?? Number.POSITIVE_INFINITY;
    return at === bt ? a.id.localeCompare(b.id) : at - bt;
  });
  const ordRatio = new Map(
    ordered.map((n, i) => [n.id, ordered.length > 1 ? i / (ordered.length - 1) : 0]),
  );
  const recOf = (n: MemoryGraphOutput["nodes"][number]): number => {
    const ts = nodeTs(n);
    const ratio =
      timed && ts !== null && minTs !== null && maxTs !== null
        ? (ts - minTs) / (maxTs - minTs)
        : (ordRatio.get(n.id) ?? 0);
    return recForRatio(ratio);
  };

  // Rings: one per populated calendar bucket when dated, else an even grid.
  let rings: DiskRing[];
  let ringIndexOf: (n: MemoryGraphOutput["nodes"][number]) => number;
  if (timed && minTs !== null && maxTs !== null) {
    const unit = chooseUnit(stamps, (maxTs - minTs) / DAY, rawNodes.length);
    const starts = populatedStarts(stamps, unit);
    const last = Math.max(1, starts.length - 1);
    const indexOfStart = new Map(starts.map((s, i) => [s, i]));
    rings = starts.map((s, i) => ({
      label: bucketLabel(s, unit),
      r: ringRadius(i),
      ratio: recForRatio(i / last),
    }));
    ringIndexOf = (n) => {
      const ts = nodeTs(n);
      if (ts === null) return starts.length - 1;
      const start = bucketStart(ts, unit);
      const exact = indexOfStart.get(start);
      if (exact !== undefined) return exact;
      // A ghost's creation bucket need not be populated by an active node.
      // Choose the nearest frozen calendar ring without changing the active
      // ring selection or introducing a new ring into the disk.
      let nearest = 0;
      for (let i = 1; i < starts.length; i += 1) {
        if (Math.abs(starts[i] - start) < Math.abs(starts[nearest] - start)) nearest = i;
      }
      return nearest;
    };
  } else {
    // Undated fallback: scale the ring count with node count so a large
    // graph still gets a proportionally large disk.
    const steps = clamp(Math.round(Math.sqrt(rawNodes.length) / 6), RING_STEPS, 24);
    rings = Array.from({ length: steps + 1 }, (_, i) => ({
      label: null,
      r: ringRadius(i),
      ratio: recForRatio(i / steps),
    }));
    ringIndexOf = (n) => {
      const rec = recOf(n);
      const idx = rings.findIndex((ring) => ring.ratio >= rec - 1e-3);
      return idx === -1 ? rings.length - 1 : idx;
    };
  }

  // Ignite staggering: a busy bucket trickles in across its band (in clusters)
  // instead of popping all at once.
  const buckets: Array<Array<MemoryGraphOutput["nodes"][number]>> = rings.map(() => []);
  for (const n of rawNodes) buckets[ringIndexOf(n)].push(n);
  const igniteByNode = new Map<string, number>();
  buckets.forEach((bucket, i) => {
    bucket.sort((a, b) => {
      const at = nodeTs(a) ?? Number.POSITIVE_INFINITY;
      const bt = nodeTs(b) ?? Number.POSITIVE_INFINITY;
      return at === bt ? a.id.localeCompare(b.id) : at - bt;
    });
    const hi = rings[i].ratio;
    const lo = i > 0 ? rings[i - 1].ratio : 0;
    const m = bucket.length;
    const clusters = Math.max(1, Math.round(m / 5));
    bucket.forEach((n, k) => {
      const c = Math.min(clusters - 1, Math.floor((k / m) * clusters));
      const jitter = ((hash(n.id) % 100) / 100 - 0.5) * (0.5 / clusters);
      const f = clamp((c + 1) / clusters + jitter, 0.02, 1);
      igniteByNode.set(n.id, lo + f * (hi - lo));
    });
  });

  // Golden-angle spiral over time order: successive notes step ~137.5° around
  // the disk, so every era fans out evenly instead of clumping where the id
  // hashes happened to land.
  const GOLDEN_ANGLE = Math.PI * (3 - Math.sqrt(5));
  const orderIndex = new Map(ordered.map((n, i) => [n.id, i]));

  // Shrink orbs as the graph grows so dense disks read as star fields, not a
  // solid ball. ~1500 notes is where the default size starts crowding.
  const sizeScale = clamp(Math.sqrt(1500 / Math.max(1, rawNodes.length)), 0.45, 1);

  const nodes: DiskNode[] = rawNodes.map((n) => {
    const rec = recOf(n);
    const ring = ringIndexOf(n);
    const tr = placeRadius(ring, n.id);
    const angle = (orderIndex.get(n.id) ?? 0) * GOLDEN_ANGLE + ((hash(n.id) % 100) / 100 - 0.5) * 0.5;
    const connectionCount = Number(n.connection_count) || 0;
    return {
      id: n.id,
      title: n.title,
      permalink: n.permalink,
      noteType: n.note_type,
      isProposal: n.entity_type === "proposal",
      isOrphan: Boolean(n.is_orphan),
      isGhost: usesGhostFallback,
      lifecycle: n.status === "deprecated" ? "deprecated" : n.status === "archived" ? "archived" : "active",
      connectionCount,
      color: colorForNote(n.note_type, Boolean(n.is_orphan)),
      ts: nodeTs(n),
      rec,
      igniteAt: usesGhostFallback ? 0 : (igniteByNode.get(n.id) ?? rec),
      tr,
      ring,
      r: Math.max(1.1, (3 + Math.sqrt(connectionCount) * 0.9 + (n.entity_type === "proposal" ? 0.6 : 0)) * sizeScale),
      x: Math.cos(angle) * tr,
      y: Math.sin(angle) * tr,
      vx: 0,
      vy: 0,
    };
  });

  const indexById = new Map(nodes.map((n, i) => [n.id, i]));
  const links: DiskLink[] = [];
  for (const e of payload.edges) {
    const a = indexById.get(e.source_id);
    const b = indexById.get(e.target_id);
    if (a !== undefined && b !== undefined && a !== b) links.push({ a, b, kind: "wikilink" });
  }
  for (const e of payload.typed_edges ?? []) {
    const a = indexById.get(e.source_id);
    const b = indexById.get(e.target_id);
    if (a !== undefined && b !== undefined && a !== b) links.push({ a, b, kind: e.kind });
  }

  // Freeze the complete active/proposal layout before placing lifecycle ghosts.
  relax(nodes, links);
  const activeById = new Map(nodes.map((n) => [n.id, n]));
  const edgeEndpoints = [...payload.edges, ...(payload.typed_edges ?? [])];
  for (const raw of ghostsToAppend) {
    const neighbors = edgeEndpoints
      .filter((edge) => edge.source_id === raw.id || edge.target_id === raw.id)
      .map((edge) => activeById.get(edge.source_id === raw.id ? edge.target_id : edge.source_id))
      .filter((node): node is DiskNode => node !== undefined)
      .sort((a, b) => hash(`${raw.id}|${a.id}`) - hash(`${raw.id}|${b.id}`) || a.id.localeCompare(b.id));
    const connectionCount = Number(raw.connection_count) || 0;
    const lifecycle = raw.status === "deprecated" ? "deprecated" : "archived";
    const r = Math.max(1.1, (3 + Math.sqrt(connectionCount) * 0.9) * sizeScale);
    const anchor = neighbors[0];
    const ring = anchor?.ring ?? ringIndexOf(raw);
    const tr = placeRadius(ring, raw.id);
    const angle = (hash(raw.id) / 0xffffffff) * Math.PI * 2;
    const distance = anchor ? anchor.r + r + 14 : tr;
    nodes.push({
      id: raw.id, title: raw.title, permalink: raw.permalink, noteType: raw.note_type,
      isProposal: raw.entity_type === "proposal", isOrphan: Boolean(raw.is_orphan), isGhost: true, lifecycle,
      connectionCount, color: colorForNote(raw.note_type, Boolean(raw.is_orphan)), ts: nodeTs(raw),
      rec: recOf(raw), igniteAt: 0, tr, ring, r,
      x: (anchor?.x ?? 0) + Math.cos(angle) * distance,
      y: (anchor?.y ?? 0) + Math.sin(angle) * distance, vx: 0, vy: 0,
    });
  }
  const allIndexById = new Map(nodes.map((n, i) => [n.id, i]));
  const allLinks: DiskLink[] = [];
  for (const edge of payload.edges) {
    const a = allIndexById.get(edge.source_id); const b = allIndexById.get(edge.target_id);
    if (a !== undefined && b !== undefined && a !== b) allLinks.push({ a, b, kind: "wikilink" });
  }
  for (const edge of payload.typed_edges ?? []) {
    const a = allIndexById.get(edge.source_id); const b = allIndexById.get(edge.target_id);
    if (a !== undefined && b !== undefined && a !== b) allLinks.push({ a, b, kind: edge.kind });
  }
  return { links: allLinks, maxTs, minTs, nodes, rings, timed };
}

/** Short-range repulsion cutoff (world units, squared). */
const REPULSE_CUTOFF_SQ = 2600;
const GRID_CELL = Math.ceil(Math.sqrt(REPULSE_CUTOFF_SQ));

/**
 * Tiny bespoke force relaxation: a dominant radial spring pins each node to
 * its date band; short-range repulsion, weak link springs, and collision
 * passes fan co-timed nodes out by angle. Runs once at build time.
 *
 * Neighbor lookups go through a uniform grid (cell = the repulsion cutoff),
 * so a tick is O(n · local density) instead of O(n²) — a 6.5k-note prod
 * graph relaxes in well under a second where the naive pairwise version
 * froze the tab.
 */
function relax(nodes: DiskNode[], links: DiskLink[]): void {
  const ticks = nodes.length > 2500 ? 120 : nodes.length > 800 ? 200 : 300;
  const grid = new Map<number, number[]>();
  const cellOf = (x: number, y: number) =>
    (Math.floor(x / GRID_CELL) + 32768) * 65536 + (Math.floor(y / GRID_CELL) + 32768);

  let alpha = 1;
  for (let tick = 0; tick < ticks && alpha > 0.02; tick += 1) {
    for (const n of nodes) {
      const r = Math.hypot(n.x, n.y) || 1;
      const k = ((n.tr - r) * 0.55 * alpha) / r;
      n.vx += n.x * k;
      n.vy += n.y * k;
    }

    grid.clear();
    nodes.forEach((n, i) => {
      const key = cellOf(n.x, n.y);
      const cell = grid.get(key);
      if (cell) cell.push(i);
      else grid.set(key, [i]);
    });

    // Repulsion + positional collision in one 3×3-neighborhood sweep; each
    // pair is handled once from the lower index.
    for (let i = 0; i < nodes.length; i += 1) {
      const a = nodes[i];
      const cx = Math.floor(a.x / GRID_CELL);
      const cy = Math.floor(a.y / GRID_CELL);
      for (let gx = cx - 1; gx <= cx + 1; gx += 1) {
        for (let gy = cy - 1; gy <= cy + 1; gy += 1) {
          const cell = grid.get((gx + 32768) * 65536 + (gy + 32768));
          if (!cell) continue;
          for (const j of cell) {
            if (j <= i) continue;
            const b = nodes[j];
            const dx = b.x - a.x;
            const dy = b.y - a.y;
            const d2 = dx * dx + dy * dy;
            if (d2 > REPULSE_CUTOFF_SQ || d2 === 0) continue;
            const f = (14 * alpha) / d2;
            a.vx -= dx * f;
            a.vy -= dy * f;
            b.vx += dx * f;
            b.vy += dy * f;
            const min = a.r + b.r + 2.5;
            if (d2 < min * min) {
              const d = Math.sqrt(d2);
              const push = (min - d) / d / 2;
              a.x -= dx * push;
              a.y -= dy * push;
              b.x += dx * push;
              b.y += dy * push;
            }
          }
        }
      }
    }

    for (const l of links) {
      const a = nodes[l.a];
      const b = nodes[l.b];
      const dx = b.x - a.x;
      const dy = b.y - a.y;
      const d = Math.hypot(dx, dy) || 1;
      const f = ((d - 26) / d) * 0.06 * alpha;
      a.vx += dx * f;
      a.vy += dy * f;
      b.vx -= dx * f;
      b.vy -= dy * f;
    }
    for (const n of nodes) {
      n.vx *= 0.38;
      n.vy *= 0.38;
      n.x += n.vx;
      n.y += n.vy;
    }
    alpha *= 0.985;
  }
}

// ── Orb rendering ────────────────────────────────────────────────────────────

/** Supersample factor for cached orb sprites (stays crisp up to ~3× zoom). */
const SPRITE_SS = 3;

/** Paint one orb (glow + body + highlight core) at (x, y) with radius r·scale. */
function drawOrb(
  ctx: CanvasRenderingContext2D,
  x: number,
  y: number,
  r: number,
  color: string,
  bodyInk: number,
  glowInk: number,
  glowMult: number,
  isProposal: boolean,
  scale: number,
): void {
  const gr = r * glowMult * scale;
  const rr = r * scale;
  const glow = ctx.createRadialGradient(x, y, 0, x, y, gr);
  glow.addColorStop(0, rgba(color, glowInk));
  glow.addColorStop(1, rgba(color, 0));
  ctx.fillStyle = glow;
  ctx.beginPath();
  ctx.arc(x, y, gr, 0, Math.PI * 2);
  ctx.fill();

  ctx.beginPath();
  if (isProposal) {
    // Proposals are diamonds; notes are orbs.
    ctx.moveTo(x, y - rr);
    ctx.lineTo(x + rr, y);
    ctx.lineTo(x, y + rr);
    ctx.lineTo(x - rr, y);
    ctx.closePath();
  } else {
    ctx.arc(x, y, rr, 0, Math.PI * 2);
  }
  ctx.fillStyle = rgba(color, bodyInk);
  ctx.fill();
  // A brighter core so dense clusters read as individual stars.
  ctx.beginPath();
  ctx.arc(x - rr * 0.25, y - rr * 0.25, rr * 0.35, 0, Math.PI * 2);
  ctx.fillStyle = `rgba(255,255,255,${(bodyInk * 0.5).toFixed(3)})`;
  ctx.fill();
}

/**
 * Cached supersampled orb sprite. Radius and ink are quantized so the cache
 * stays bounded (colors × ~radius steps × 9 ink levels × 2 glyphs); birth and
 * age transitions step through quantized levels, which is invisible at orb
 * sizes.
 */
function orbSprite(
  cache: Map<string, HTMLCanvasElement>,
  color: string,
  r: number,
  ink: number,
  glowMult: number,
  isProposal: boolean,
): HTMLCanvasElement {
  const rq = Math.max(0.5, Math.round(r * 2) / 2);
  const inkq = Math.round(clamp(ink, 0, 1) * 8) / 8;
  const key = `${color}|${rq}|${inkq}|${isProposal ? "p" : "n"}|${glowMult}`;
  const hit = cache.get(key);
  if (hit) return hit;
  if (cache.size > 1500) cache.clear();
  const half = Math.ceil(rq * glowMult * SPRITE_SS) + 2;
  const sprite = document.createElement("canvas");
  sprite.width = half * 2;
  sprite.height = half * 2;
  const g = sprite.getContext("2d");
  if (g) drawOrb(g, half, half, rq, color, inkq, inkq * 0.38, glowMult, isProposal, SPRITE_SS);
  cache.set(key, sprite);
  return sprite;
}

// ── Component ────────────────────────────────────────────────────────────────

interface MemoryGraphCanvasProps {
  /** Project slug in `owner/repo` form (same as `MemoryPage` uses for MCP calls). */
  projectSlug: string;
  /** Bumping this re-issues the graph fetch without unmounting. */
  reloadKey?: number;
  /** Called when the user clicks a node. */
  onSelectNote?: (permalink: string) => void;
}

type FetchState =
  | { status: "loading" }
  | { status: "empty" }
  | { status: "error"; error: string }
  | { status: "ready"; disk: DiskModel; inactiveOmitted: number };

interface Camera {
  k: number;
  x: number;
  y: number;
}

/** Fit camera for a disk of radius `outer`, never zoomed out past the full disk. */
function fitCamera(w: number, h: number, outer: number, fullOuter: number): Camera {
  if (w <= 0 || h <= 0) return { k: 1, x: w / 2, y: h / 2 };
  const kFor = (r: number) => {
    const span = (r + 30) * 2;
    return Math.min((w - FIT_PADDING * 2) / span, (h - FIT_PADDING * 2) / span, FIT_ZOOM_CAP);
  };
  const k = clamp(Math.max(kFor(outer), kFor(fullOuter)), ZOOM_MIN, ZOOM_MAX);
  return { k, x: w / 2, y: h / 2 + h * 0.04 };
}

/** The frozen active disk extent used by camera fitting (exposed for layout invariants). */
export function memoryGraphCameraFitRadius(disk: DiskModel): number {
  return disk.rings[disk.rings.length - 1]?.r ?? RING_OUTER;
}

const lifecycleGhostPreferenceKey = (projectSlug: string) =>
  `djinn:memory-graph:lifecycle-ghosts:${projectSlug}`;

/** Missing or inaccessible preferences intentionally fail open to ghosts. */
function readLifecycleGhostPreference(projectSlug: string): boolean {
  try {
    return window.localStorage.getItem(lifecycleGhostPreferenceKey(projectSlug)) !== "0";
  } catch {
    return true;
  }
}

export function MemoryGraphCanvas({ projectSlug, reloadKey, onSelectNote }: MemoryGraphCanvasProps) {
  const [state, setState] = useState<FetchState>({ status: "loading" });
  const [lifecycleGhosts, setLifecycleGhosts] = useState(true);
  const [preferenceProject, setPreferenceProject] = useState<string | null>(null);
  const containerRef = useRef<HTMLDivElement | null>(null);
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const sliderRef = useRef<HTMLInputElement | null>(null);
  const dateRef = useRef<HTMLSpanElement | null>(null);
  const [playing, setPlaying] = useState(false);
  const [finished, setFinished] = useState(false);

  // Animation state lives in refs — the rAF loop reads them without re-rendering.
  const revealRef = useRef(0);
  const playingRef = useRef(false);
  const appearRef = useRef<Float32Array>(new Float32Array(0));
  const ringGrowRef = useRef<Float32Array>(new Float32Array(0));
  const labelFadeRef = useRef<Float32Array>(new Float32Array(0));
  const cameraRef = useRef<Camera>({ k: 1, x: 0, y: 0 });
  const userCamRef = useRef(false);
  const hoverRef = useRef<number>(-1);
  const hoverRingRef = useRef<number>(-1);
  const pointerRef = useRef<{ x: number; y: number } | null>(null);

  // ── Fetch memory_graph on project / reload change ──────────────────────────
  useEffect(() => {
    if (preferenceProject !== projectSlug) {
      setLifecycleGhosts(readLifecycleGhostPreference(projectSlug));
      setPreferenceProject(projectSlug);
      return;
    }

    let cancelled = false;

    (async () => {
      try {
        setState({ status: "loading" });
        const input: MemoryGraphInput = lifecycleGhosts
          ? {
              project: projectSlug,
              statuses: ["active", "archived", "deprecated"],
              lifecycle_limit: 500,
            }
          : { project: projectSlug };
        const raw = await callMcpTool("memory_graph", input);
        if (cancelled) return;
        const payload = parseMemoryGraphResponse(raw);
        if (!payload || payload.nodes.length === 0) {
          setState({ status: "empty" });
          return;
        }
        const disk = buildMemoryGraphDisk(payload);
        if (cancelled) return;
        appearRef.current = new Float32Array(disk.nodes.length);
        ringGrowRef.current = new Float32Array(disk.rings.length);
        labelFadeRef.current = new Float32Array(disk.rings.length);
        revealRef.current = 0;
        playingRef.current = true;
        userCamRef.current = false;
        setPlaying(true);
        setFinished(false);
        setState({
          disk,
          inactiveOmitted: payload.lifecycle_summary?.inactive_omitted ?? 0,
          status: "ready",
        });
      } catch (err) {
        if (cancelled) return;
        setState({ error: err instanceof Error ? err.message : String(err), status: "error" });
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [lifecycleGhosts, preferenceProject, projectSlug, reloadKey]);

  const disk = state.status === "ready" ? state.disk : null;

  // ── Render loop ─────────────────────────────────────────────────────────────
  useEffect(() => {
    if (!disk) return;
    const canvas = canvasRef.current;
    const container = containerRef.current;
    if (!canvas || !container) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    const fullOuter = memoryGraphCameraFitRadius(disk);
    // Ghosts render independently of playback. Keeping this set separate is
    // important: lifecycle payloads must not advance the active reveal count
    // or frontier-relative ink calculation.
    const activeNodes = disk.nodes.filter((node) => !node.isGhost);
    let raf = 0;
    let last = performance.now();
    let size = { h: container.clientHeight, w: container.clientWidth };

    const resize = () => {
      size = { h: container.clientHeight, w: container.clientWidth };
      const dpr = window.devicePixelRatio || 1;
      canvas.width = Math.max(1, Math.round(size.w * dpr));
      canvas.height = Math.max(1, Math.round(size.h * dpr));
      canvas.style.width = `${size.w}px`;
      canvas.style.height = `${size.h}px`;
    };
    resize();
    const ro = new ResizeObserver(resize);
    ro.observe(container);
    cameraRef.current = fitCamera(size.w, size.h, fullOuter, fullOuter);

    // Orb sprite cache for this graph; tighter glow on dense disks.
    const sprites = new Map<string, HTMLCanvasElement>();
    const glowMult = disk.nodes.length > 1500 ? 2.4 : 3.4;

    // Static starfield, seeded so it never re-rolls between frames.
    const rand = createSeededRandom(7);
    const stars = Array.from({ length: 160 }, () => ({
      a: 0.05 + rand() * 0.3,
      phase: rand() * Math.PI * 2,
      speed: 0.3 + rand() * 1.2,
      x: rand(),
      y: rand(),
    }));

    const frame = (now: number) => {
      const dt = Math.min(64, now - last);
      last = now;

      // Advance the playhead.
      if (playingRef.current) {
        revealRef.current = Math.min(1, revealRef.current + dt / PLAYBACK_MS);
        if (revealRef.current >= 1) {
          playingRef.current = false;
          setPlaying(false);
          setFinished(true);
        }
        if (sliderRef.current) sliderRef.current.value = String(Math.round(revealRef.current * 1000));
      }
      const reveal = revealRef.current;

      // Playhead date readout.
      if (dateRef.current) {
        if (disk.timed && disk.minTs !== null && disk.maxTs !== null) {
          const ts = disk.minTs + reveal * (disk.maxTs - disk.minTs);
          dateRef.current.textContent = new Date(ts * 1000).toLocaleDateString(undefined, {
            day: "numeric",
            month: "short",
            timeZone: "UTC",
            year: "numeric",
          });
        } else {
          const visible = activeNodes.filter((n) => n.igniteAt <= reveal + 1e-3).length;
          dateRef.current.textContent = `${visible} / ${activeNodes.length} notes`;
        }
      }

      // Camera: during playback (and until the user grabs it), fit the revealed extent.
      if (!userCamRef.current) {
        const revealedOuter = Math.max(
          RING_CORE + RING_BAND,
          RING_CORE + smooth(reveal) * (fullOuter - RING_CORE) + RING_BAND * 0.6,
        );
        const target = fitCamera(size.w, size.h, revealedOuter, fullOuter);
        const cam = cameraRef.current;
        cam.k += (target.k - cam.k) * 0.06;
        cam.x += (target.x - cam.x) * 0.06;
        cam.y += (target.y - cam.y) * 0.06;
      }
      const cam = cameraRef.current;

      const dpr = window.devicePixelRatio || 1;
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      ctx.clearRect(0, 0, size.w, size.h);

      // Starfield (screen space, gentle twinkle).
      for (const s of stars) {
        const tw = 0.65 + 0.35 * Math.sin((now / 1000) * s.speed + s.phase);
        ctx.fillStyle = `rgba(226,232,240,${(s.a * tw).toFixed(3)})`;
        ctx.fillRect(s.x * size.w, s.y * size.h, 1, 1);
      }

      ctx.save();
      ctx.translate(cam.x, cam.y);
      ctx.scale(cam.k, cam.k);

      // Frontier-relative recency: a lone frontier note still reads as fresh
      // even when the playhead has left empty space behind it.
      let frontier = LEAD_IN;
      for (const n of activeNodes) if (n.igniteAt <= reveal + 1e-3) frontier = Math.max(frontier, n.rec);
      const styleInk = (rec: number) => recencyInk(clamp(rec / Math.max(frontier, LEAD_IN), 0, 1));

      // Rings: a ring is laid one band ahead of the playhead and grows out
      // from its inner neighbour instead of popping. Hovering inside a ring's
      // band lights it up (wash + brighter outline) and surfaces its date.
      const grow = ringGrowRef.current;
      const hoverRing = hoverRingRef.current;
      disk.rings.forEach((ring, i) => {
        const layAt = i > 0 ? disk.rings[i - 1].ratio : 0.02;
        const seen = reveal >= layAt;
        grow[i] = clamp(grow[i] + (seen ? 1 : -1) * (dt / 450), 0, 1);
        if (grow[i] <= 0.01) return;
        const inner = i > 0 ? disk.rings[i - 1].r : RING_CORE * 0.4;
        const r = inner + smooth(grow[i]) * (ring.r - inner);
        if (hoverRing === i) {
          ctx.beginPath();
          ctx.arc(0, 0, r, 0, Math.PI * 2);
          ctx.arc(0, 0, Math.max(0, inner), 0, Math.PI * 2, true);
          ctx.fillStyle = "rgba(167,139,250,0.035)";
          ctx.fill();
        }
        ctx.beginPath();
        ctx.arc(0, 0, r, 0, Math.PI * 2);
        ctx.strokeStyle = `rgba(167,139,250,${((0.13 + (hoverRing === i ? 0.14 : 0)) * grow[i]).toFixed(3)})`;
        ctx.lineWidth = 1 / cam.k;
        ctx.setLineDash([]);
        ctx.stroke();
      });

      // Links: quiet threads; a hovered node's links light up. Quiet edges
      // batch into one path per style so a 10k-edge graph costs a handful of
      // stroke calls, not one per edge.
      const hover = hoverRef.current;
      const revealed = (idx: number) => disk.nodes[idx].igniteAt <= reveal + 1e-3;
      const hotLinks: DiskLink[] = [];
      const quietByStyle = new Map<string, DiskLink[]>();
      for (const l of disk.links) {
        if (!revealed(l.a) || !revealed(l.b)) continue;
        if (hover === l.a || hover === l.b) {
          hotLinks.push(l);
          continue;
        }
        const bucket = quietByStyle.get(l.kind);
        if (bucket) bucket.push(l);
        else quietByStyle.set(l.kind, [l]);
      }
      for (const [kind, bucket] of quietByStyle) {
        const typed = TYPED_EDGE_STYLES[kind];
        ctx.beginPath();
        for (const l of bucket) {
          ctx.moveTo(disk.nodes[l.a].x, disk.nodes[l.a].y);
          ctx.lineTo(disk.nodes[l.b].x, disk.nodes[l.b].y);
        }
        if (typed) {
          ctx.strokeStyle = rgba(typed.color, 0.3);
          ctx.lineWidth = 0.9 / cam.k;
          ctx.setLineDash(typed.dashed ? [4 / cam.k, 4 / cam.k] : []);
        } else {
          ctx.strokeStyle = "rgba(148,163,184,0.14)";
          ctx.lineWidth = 0.7 / cam.k;
          ctx.setLineDash([]);
        }
        ctx.stroke();
      }
      for (const l of hotLinks) {
        const typed = TYPED_EDGE_STYLES[l.kind];
        ctx.beginPath();
        ctx.moveTo(disk.nodes[l.a].x, disk.nodes[l.a].y);
        ctx.lineTo(disk.nodes[l.b].x, disk.nodes[l.b].y);
        if (typed) {
          ctx.strokeStyle = rgba(typed.color, 0.85);
          ctx.lineWidth = 1.4 / cam.k;
          ctx.setLineDash(typed.dashed ? [4 / cam.k, 4 / cam.k] : []);
        } else {
          ctx.strokeStyle = "rgba(148,163,184,0.7)";
          ctx.lineWidth = 1.2 / cam.k;
          ctx.setLineDash([]);
        }
        ctx.stroke();
      }
      ctx.setLineDash([]);

      // Nodes: glow + core, aged ink, warp-in on ignite. Steady-state nodes
      // draw from a sprite cache (one drawImage each) so big graphs never pay
      // for per-node gradient allocation; only the hovered node paints live.
      const appear = appearRef.current;
      disk.nodes.forEach((n, i) => {
        const on = n.igniteAt <= reveal + 1e-3;
        appear[i] = clamp(appear[i] + (on ? 1 : -1) * (dt / 420), 0, 1);
        if (appear[i] <= 0.01) return;
        const birth = smooth(appear[i]);
        // Orphans dim to 55% so a mostly-orphan graph doesn't drown in red.
        const ink = styleInk(n.rec) * birth * (n.isOrphan ? 0.55 : 1);
        const hot = hover === i;
        const r = n.r * (0.35 + 0.65 * birth) * (hot ? 1.25 : 1);

        if (hot) {
          drawOrb(ctx, n.x, n.y, r, n.color, clamp(ink + 0.15, 0, 1), 0.6 * ink, glowMult, n.isProposal, 1);
          return;
        }
        const sprite = orbSprite(sprites, n.color, r, ink, glowMult, n.isProposal);
        const w = sprite.width / SPRITE_SS;
        ctx.drawImage(sprite, n.x - w / 2, n.y - w / 2, w, w);
      });

      ctx.restore();

      // Ring date labels (screen space, top of each ring), thinned by gap.
      // Labels ride along during the build-up, then fade out once the map is
      // fully rendered — after that, a ring surfaces its date only while the
      // pointer is inside its band.
      const labelFade = labelFadeRef.current;
      const building = reveal < 1 - 1e-3;
      ctx.font = "10px JetBrains Mono, ui-monospace, monospace";
      ctx.textAlign = "center";
      let lastLabelY = Number.NEGATIVE_INFINITY;
      for (let i = disk.rings.length - 1; i >= 0; i -= 1) {
        const ring = disk.rings[i];
        if (!ring.label) continue;
        const wanted = building ? grow[i] > 0.5 : hoverRing === i;
        labelFade[i] = clamp(labelFade[i] + (wanted ? 1 : -1) * (dt / 250), 0, 1);
        if (labelFade[i] <= 0.02) continue;
        const y = cam.y - ring.r * cam.k - 5;
        // Inner rings sit lower on screen; skip any label that would crowd the
        // previous (outer) one.
        if (y - lastLabelY < 14 && i !== disk.rings.length - 1 && building) continue;
        lastLabelY = y;
        ctx.fillStyle = `rgba(161,161,180,${(0.8 * labelFade[i] * grow[i]).toFixed(3)})`;
        ctx.fillText(ring.label, cam.x, y);
      }

      // Hover pill (screen space).
      if (hover >= 0 && appear[hover] > 0.5) {
        const n = disk.nodes[hover];
        const sx = n.x * cam.k + cam.x;
        const sy = n.y * cam.k + cam.y;
        const meta = `${n.noteType}${n.ts ? ` · ${formatDay(n.ts)}` : ""}${n.isOrphan ? " · orphan" : ""}`;
        ctx.font = "11px JetBrains Mono, ui-monospace, monospace";
        const w = Math.max(ctx.measureText(n.title).width, ctx.measureText(meta).width) + 16;
        const bx = clamp(sx + 12, 4, size.w - w - 4);
        const by = clamp(sy - 34, 4, size.h - 40);
        ctx.fillStyle = "rgba(18,18,28,0.92)";
        ctx.strokeStyle = "#2d2d3d";
        ctx.beginPath();
        ctx.roundRect(bx, by, w, 34, 6);
        ctx.fill();
        ctx.stroke();
        ctx.textAlign = "left";
        ctx.fillStyle = "#f5f5f7";
        ctx.fillText(n.title, bx + 8, by + 14);
        ctx.fillStyle = rgba(n.color, 0.95);
        ctx.fillText(meta, bx + 8, by + 27);
        ctx.textAlign = "center";
      }

      // Hit-test for the next frame's hover state (node, then containing ring).
      const p = pointerRef.current;
      if (p) {
        const wx = (p.x - cam.x) / cam.k;
        const wy = (p.y - cam.y) / cam.k;
        let best = -1;
        let bestD = Math.max(10 / cam.k, 6);
        disk.nodes.forEach((n, i) => {
          if (appear[i] <= 0.5) return;
          const d = Math.hypot(n.x - wx, n.y - wy) - n.r;
          if (d < bestD) {
            bestD = d;
            best = i;
          }
        });
        hoverRef.current = best;
        canvas.style.cursor = best >= 0 ? "pointer" : "grab";

        const wr = Math.hypot(wx, wy);
        let ringHit = -1;
        for (let i = 0; i < disk.rings.length; i += 1) {
          if (wr <= disk.rings[i].r && grow[i] > 0.5) {
            ringHit = i;
            break;
          }
        }
        hoverRingRef.current = ringHit;
      } else {
        hoverRingRef.current = -1;
      }

      raf = requestAnimationFrame(frame);
    };

    raf = requestAnimationFrame(frame);
    return () => {
      cancelAnimationFrame(raf);
      ro.disconnect();
    };
  }, [disk]);

  // ── Pointer interaction: hover, click, drag-pan, wheel-zoom ────────────────
  const onPointerDown = useCallback((e: React.PointerEvent<HTMLCanvasElement>) => {
    const el = e.currentTarget;
    el.setPointerCapture(e.pointerId);
    const start = { camX: cameraRef.current.x, camY: cameraRef.current.y, moved: false, x: e.clientX, y: e.clientY };

    const move = (ev: PointerEvent) => {
      const dx = ev.clientX - start.x;
      const dy = ev.clientY - start.y;
      if (Math.hypot(dx, dy) > 3) {
        start.moved = true;
        userCamRef.current = true;
        cameraRef.current.x = start.camX + dx;
        cameraRef.current.y = start.camY + dy;
      }
    };
    const up = () => {
      el.removeEventListener("pointermove", move);
      el.removeEventListener("pointerup", up);
      if (!start.moved && hoverRef.current >= 0 && disk) {
        onSelectNote?.(disk.nodes[hoverRef.current].permalink);
      }
    };
    el.addEventListener("pointermove", move);
    el.addEventListener("pointerup", up);
  }, [disk, onSelectNote]);

  const onPointerMove = useCallback((e: React.PointerEvent<HTMLCanvasElement>) => {
    const rect = e.currentTarget.getBoundingClientRect();
    pointerRef.current = { x: e.clientX - rect.left, y: e.clientY - rect.top };
  }, []);

  const onPointerLeave = useCallback(() => {
    pointerRef.current = null;
    hoverRef.current = -1;
    hoverRingRef.current = -1;
  }, []);

  const onWheel = useCallback((e: React.WheelEvent<HTMLCanvasElement>) => {
    const rect = e.currentTarget.getBoundingClientRect();
    const px = e.clientX - rect.left;
    const py = e.clientY - rect.top;
    const cam = cameraRef.current;
    const k = clamp(cam.k * Math.exp(-e.deltaY * 0.0015), ZOOM_MIN, ZOOM_MAX);
    // Zoom about the cursor.
    cam.x = px - ((px - cam.x) / cam.k) * k;
    cam.y = py - ((py - cam.y) / cam.k) * k;
    cam.k = k;
    userCamRef.current = true;
  }, []);

  // ── Playback controls ───────────────────────────────────────────────────────
  const togglePlay = useCallback(() => {
    if (revealRef.current >= 1) {
      revealRef.current = 0;
      userCamRef.current = false;
      setFinished(false);
      playingRef.current = true;
      setPlaying(true);
      return;
    }
    playingRef.current = !playingRef.current;
    setPlaying(playingRef.current);
  }, []);

  const onScrub = useCallback((e: React.ChangeEvent<HTMLInputElement>) => {
    revealRef.current = Number(e.target.value) / 1000;
    playingRef.current = false;
    setPlaying(false);
    setFinished(revealRef.current >= 1);
  }, []);

  const onLifecycleGhostsChange = useCallback((e: React.ChangeEvent<HTMLInputElement>) => {
    const enabled = e.target.checked;
    try {
      window.localStorage.setItem(lifecycleGhostPreferenceKey(projectSlug), enabled ? "1" : "0");
      setLifecycleGhosts(enabled);
    } catch {
      // Storage failures fail open so they cannot make the graph unusable.
      setLifecycleGhosts(true);
    }
  }, [projectSlug]);

  const noteTypes = disk
    ? [...new Set(disk.nodes.filter((n) => !n.isOrphan).map((n) => n.noteType))].slice(0, 6)
    : [];

  return (
    <div
      ref={containerRef}
      className="relative h-full min-h-0 w-full overflow-hidden"
      style={{ background: CANVAS_BACKGROUND }}
      data-testid="memory-graph-canvas"
    >
      <canvas
        ref={canvasRef}
        className="absolute inset-0"
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerLeave={onPointerLeave}
        onWheel={onWheel}
      />

      <label className="absolute right-3 top-3 z-10 flex items-center gap-2 rounded-full border border-[#2d2d3d] bg-[#0a0a10]/85 px-3 py-2 text-xs text-zinc-300 backdrop-blur">
        <input
          type="checkbox"
          checked={lifecycleGhosts}
          onChange={onLifecycleGhostsChange}
          className="accent-violet-400"
        />
        Show lifecycle ghosts
      </label>

      {state.status === "ready" && lifecycleGhosts && state.inactiveOmitted > 0 && (
        <div className="absolute right-3 top-14 z-10 rounded-full border border-[#2d2d3d] bg-[#0a0a10]/85 px-3 py-1.5 font-mono text-[11px] text-zinc-400 backdrop-blur">
          500 shown · {state.inactiveOmitted} older hidden
        </div>
      )}

      {/* Legend */}
      {disk && (
        <>
          <div className="pointer-events-none absolute bottom-3 left-3 z-10 flex flex-wrap items-center gap-x-3 gap-y-1">
            {noteTypes.map((t) => (
              <span key={t} className="flex items-center gap-1.5 text-[11px] text-zinc-400">
                <span className="h-2 w-2 rounded-full" style={{ background: colorForNote(t, false) }} />
                {t}
              </span>
            ))}
          </div>

          {/* Playback scrubber */}
          <div className="absolute bottom-3 left-1/2 z-10 flex -translate-x-1/2 items-center gap-3 rounded-full border border-[#2d2d3d] bg-[#0a0a10]/85 px-4 py-2 backdrop-blur">
            <button
              type="button"
              onClick={togglePlay}
              className="flex h-6 w-6 items-center justify-center rounded-full text-zinc-300 hover:bg-zinc-800/60 hover:text-zinc-100"
              aria-label={finished ? "Replay" : playing ? "Pause" : "Play"}
            >
              <HugeiconsIcon icon={finished ? RefreshIcon : playing ? PauseIcon : PlayIcon} className="h-3.5 w-3.5" />
            </button>
            <input
              ref={sliderRef}
              type="range"
              min={0}
              max={1000}
              defaultValue={0}
              onChange={onScrub}
              className="h-1 w-44 accent-violet-400"
              aria-label="Timeline position"
            />
            <span ref={dateRef} className="w-24 text-right font-mono text-[11px] text-violet-300" />
          </div>
        </>
      )}

      {/* Loading / error / empty overlays */}
      {state.status !== "ready" && (
        <div className="pointer-events-none absolute inset-0 flex items-center justify-center">
          <div className="max-w-sm rounded-lg border border-[#2d2d3d] bg-[#0a0a10]/85 px-5 py-4 text-center backdrop-blur">
            {state.status === "loading" && <p className="text-sm text-zinc-400">Loading memory graph…</p>}
            {state.status === "empty" && (
              <p className="text-sm text-zinc-400">No notes yet — the graph builds here as this project learns.</p>
            )}
            {state.status === "error" && (
              <>
                <span className="mx-auto flex h-10 w-10 items-center justify-center rounded-full bg-red-500/15 text-red-400">
                  <HugeiconsIcon icon={AlertCircleIcon} className="h-5 w-5" />
                </span>
                <p className="mt-3 text-sm font-medium text-zinc-200">Couldn&apos;t load the graph</p>
                <p className="mt-1 max-w-sm text-xs text-zinc-400">{state.error}</p>
              </>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
