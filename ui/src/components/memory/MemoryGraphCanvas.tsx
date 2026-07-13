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
import type { MemoryGraphOutput } from "@/api/generated/mcp-tools.gen";
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

interface DiskNode {
  id: string;
  title: string;
  permalink: string;
  noteType: string;
  isProposal: boolean;
  isOrphan: boolean;
  connectionCount: number;
  color: string;
  ts: number | null;
  /** Recency 0 (oldest) → 1 (newest); drives radius, ink, and reveal order. */
  rec: number;
  /** Reveal coordinate this node ignites at (staggered within its date band). */
  igniteAt: number;
  /** Time-anchored target radius (inside its ring's band). */
  tr: number;
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

interface DiskModel {
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

/** Aim for ~5–12 rings (growing ~log2 with span), snapped to a calendar interval. */
function chooseUnit(stamps: number[], spanDays: number): (typeof UNITS)[number] {
  const target = clamp(Math.round(4 + Math.log2(Math.max(1, spanDays / 60))), 5, 12);
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

function buildDisk(payload: MemoryGraphOutput): DiskModel {
  const rawNodes = payload.nodes;
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
    const unit = chooseUnit(stamps, (maxTs - minTs) / DAY);
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
      return ts !== null ? (indexOfStart.get(bucketStart(ts, unit)) ?? starts.length - 1) : starts.length - 1;
    };
  } else {
    rings = Array.from({ length: RING_STEPS + 1 }, (_, i) => ({
      label: null,
      r: ringRadius(i),
      ratio: recForRatio(i / RING_STEPS),
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

  const nodes: DiskNode[] = rawNodes.map((n) => {
    const rec = recOf(n);
    const tr = placeRadius(ringIndexOf(n), n.id);
    const angle = (orderIndex.get(n.id) ?? 0) * GOLDEN_ANGLE + ((hash(n.id) % 100) / 100 - 0.5) * 0.5;
    const connectionCount = Number(n.connection_count) || 0;
    return {
      id: n.id,
      title: n.title,
      permalink: n.permalink,
      noteType: n.note_type,
      isProposal: n.entity_type === "proposal",
      isOrphan: Boolean(n.is_orphan),
      connectionCount,
      color: colorForNote(n.note_type, Boolean(n.is_orphan)),
      ts: nodeTs(n),
      rec,
      igniteAt: igniteByNode.get(n.id) ?? rec,
      tr,
      r: 3 + Math.sqrt(connectionCount) * 0.9 + (n.entity_type === "proposal" ? 0.6 : 0),
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

  relax(nodes, links);
  return { links, maxTs, minTs, nodes, rings, timed };
}

/**
 * Tiny bespoke force relaxation: a dominant radial spring pins each node to
 * its date band; short-range repulsion, weak link springs, and collision
 * passes fan co-timed nodes out by angle. Runs once at build time.
 */
function relax(nodes: DiskNode[], links: DiskLink[]): void {
  let alpha = 1;
  for (let tick = 0; tick < 300 && alpha > 0.02; tick += 1) {
    for (const n of nodes) {
      const r = Math.hypot(n.x, n.y) || 1;
      const k = ((n.tr - r) * 0.55 * alpha) / r;
      n.vx += n.x * k;
      n.vy += n.y * k;
    }
    for (let i = 0; i < nodes.length; i += 1) {
      for (let j = i + 1; j < nodes.length; j += 1) {
        const a = nodes[i];
        const b = nodes[j];
        const dx = b.x - a.x;
        const dy = b.y - a.y;
        const d2 = dx * dx + dy * dy;
        if (d2 > 2600 || d2 === 0) continue;
        const f = (14 * alpha) / d2;
        a.vx -= dx * f;
        a.vy -= dy * f;
        b.vx += dx * f;
        b.vy += dy * f;
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
    // Positional collision pass.
    for (let i = 0; i < nodes.length; i += 1) {
      for (let j = i + 1; j < nodes.length; j += 1) {
        const a = nodes[i];
        const b = nodes[j];
        const min = a.r + b.r + 2.5;
        const dx = b.x - a.x;
        const dy = b.y - a.y;
        const d = Math.hypot(dx, dy);
        if (d >= min || d === 0) continue;
        const push = (min - d) / d / 2;
        a.x -= dx * push;
        a.y -= dy * push;
        b.x += dx * push;
        b.y += dy * push;
      }
    }
    alpha *= 0.985;
  }
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
  | { status: "ready"; disk: DiskModel };

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

export function MemoryGraphCanvas({ projectSlug, reloadKey, onSelectNote }: MemoryGraphCanvasProps) {
  const [state, setState] = useState<FetchState>({ status: "loading" });
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
    let cancelled = false;

    (async () => {
      try {
        setState({ status: "loading" });
        const raw = await callMcpTool("memory_graph", { project: projectSlug });
        if (cancelled) return;
        const payload = parseMemoryGraphResponse(raw);
        if (!payload || payload.nodes.length === 0) {
          setState({ status: "empty" });
          return;
        }
        const disk = buildDisk(payload);
        if (cancelled) return;
        appearRef.current = new Float32Array(disk.nodes.length);
        ringGrowRef.current = new Float32Array(disk.rings.length);
        labelFadeRef.current = new Float32Array(disk.rings.length);
        revealRef.current = 0;
        playingRef.current = true;
        userCamRef.current = false;
        setPlaying(true);
        setFinished(false);
        setState({ disk, status: "ready" });
      } catch (err) {
        if (cancelled) return;
        setState({ error: err instanceof Error ? err.message : String(err), status: "error" });
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [projectSlug, reloadKey]);

  const disk = state.status === "ready" ? state.disk : null;

  // ── Render loop ─────────────────────────────────────────────────────────────
  useEffect(() => {
    if (!disk) return;
    const canvas = canvasRef.current;
    const container = containerRef.current;
    if (!canvas || !container) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    const fullOuter = disk.rings[disk.rings.length - 1]?.r ?? RING_OUTER;
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
          const visible = disk.nodes.filter((n) => n.igniteAt <= reveal + 1e-3).length;
          dateRef.current.textContent = `${visible} / ${disk.nodes.length} notes`;
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
      for (const n of disk.nodes) if (n.igniteAt <= reveal + 1e-3) frontier = Math.max(frontier, n.rec);
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

      // Links: quiet threads; a hovered node's links light up.
      const hover = hoverRef.current;
      const revealed = (idx: number) => disk.nodes[idx].igniteAt <= reveal + 1e-3;
      for (const l of disk.links) {
        if (!revealed(l.a) || !revealed(l.b)) continue;
        const a = disk.nodes[l.a];
        const b = disk.nodes[l.b];
        const hot = hover === l.a || hover === l.b;
        const typed = TYPED_EDGE_STYLES[l.kind];
        ctx.beginPath();
        ctx.moveTo(a.x, a.y);
        ctx.lineTo(b.x, b.y);
        if (typed) {
          ctx.strokeStyle = rgba(typed.color, hot ? 0.85 : 0.3);
          ctx.lineWidth = (hot ? 1.4 : 0.9) / cam.k;
          ctx.setLineDash(typed.dashed ? [4 / cam.k, 4 / cam.k] : []);
        } else {
          ctx.strokeStyle = `rgba(148,163,184,${hot ? 0.7 : 0.14})`;
          ctx.lineWidth = (hot ? 1.2 : 0.7) / cam.k;
          ctx.setLineDash([]);
        }
        ctx.stroke();
      }
      ctx.setLineDash([]);

      // Nodes: glow + core, aged ink, warp-in on ignite.
      const appear = appearRef.current;
      disk.nodes.forEach((n, i) => {
        const on = n.igniteAt <= reveal + 1e-3;
        appear[i] = clamp(appear[i] + (on ? 1 : -1) * (dt / 420), 0, 1);
        if (appear[i] <= 0.01) return;
        const birth = smooth(appear[i]);
        const ink = styleInk(n.rec) * birth;
        const hot = hover === i;
        const r = n.r * (0.35 + 0.65 * birth) * (hot ? 1.25 : 1);

        const glow = ctx.createRadialGradient(n.x, n.y, 0, n.x, n.y, r * 3.4);
        glow.addColorStop(0, rgba(n.color, ink * (hot ? 0.6 : 0.38)));
        glow.addColorStop(1, rgba(n.color, 0));
        ctx.fillStyle = glow;
        ctx.beginPath();
        ctx.arc(n.x, n.y, r * 3.4, 0, Math.PI * 2);
        ctx.fill();

        ctx.beginPath();
        if (n.isProposal) {
          // Proposals are diamonds; notes are orbs.
          ctx.moveTo(n.x, n.y - r);
          ctx.lineTo(n.x + r, n.y);
          ctx.lineTo(n.x, n.y + r);
          ctx.lineTo(n.x - r, n.y);
          ctx.closePath();
        } else {
          ctx.arc(n.x, n.y, r, 0, Math.PI * 2);
        }
        ctx.fillStyle = rgba(n.color, clamp(ink + (hot ? 0.15 : 0), 0, 1));
        ctx.fill();
        // A brighter core so dense clusters read as individual stars.
        ctx.beginPath();
        ctx.arc(n.x - r * 0.25, n.y - r * 0.25, r * 0.35, 0, Math.PI * 2);
        ctx.fillStyle = `rgba(255,255,255,${(ink * 0.5).toFixed(3)})`;
        ctx.fill();
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
