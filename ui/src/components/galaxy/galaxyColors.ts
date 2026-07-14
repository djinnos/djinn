/**
 * galaxyColors — color scales and density compensation for the galaxy view.
 *
 * Ported from codebase-memory-mcp's graph-ui (MIT, © 2025 DeusData):
 * `src/ui/layout3d.c` (stellar scale) and `graph-ui/src/lib/density.ts`
 * (density compensation + glow boost). Kept numerically identical to their
 * shipped baseline — it is the proven look; resist re-tuning constants here.
 *
 * The heat scale (cognitive-complexity mode) is djinn's own addition.
 */

export type Rgb = [number, number, number];

// ── Stellar spectral scale (degree → color) ─────────────────────────────────
//
// Hertzsprung–Russell intuition: dim red dwarfs = leaves, blue giants =
// mega-hubs. Color IS the importance encoding.

const STELLAR_STOPS: Array<{ maxDegree: number; hex: number }> = [
  { maxDegree: 1, hex: 0xff6050 }, // M — red dwarf
  { maxDegree: 3, hex: 0xff8855 }, // late K — orange-red
  { maxDegree: 5, hex: 0xffa060 }, // K — orange
  { maxDegree: 8, hex: 0xffc070 }, // early K — warm orange
  { maxDegree: 12, hex: 0xffe080 }, // G — yellow (Sun-like)
  { maxDegree: 18, hex: 0xfff0c0 }, // F — yellow-white
  { maxDegree: 25, hex: 0xfff8e8 }, // late A — warm white
  { maxDegree: 35, hex: 0xe8e8ff }, // A — white-blue
  { maxDegree: 50, hex: 0xc0d0ff }, // B — blue-white
];
const STELLAR_TOP = 0x80a0ff; // O — blue giant

function hexToRgb(hex: number): Rgb {
  return [((hex >> 16) & 0xff) / 255, ((hex >> 8) & 0xff) / 255, (hex & 0xff) / 255];
}

const STELLAR_RGB = STELLAR_STOPS.map((s) => ({
  maxDegree: s.maxDegree,
  color: hexToRgb(s.hex),
}));
const STELLAR_TOP_RGB = hexToRgb(STELLAR_TOP);

export function stellarColor(degree: number): Rgb {
  for (const stop of STELLAR_RGB) {
    if (degree <= stop.maxDegree) return stop.color;
  }
  return STELLAR_TOP_RGB;
}

// ── Heat scale (djinn cognitive-complexity mode) ────────────────────────────

const HEAT_LOW: Rgb = [0.204, 0.827, 0.6]; // emerald-400
const HEAT_MID: Rgb = [0.918, 0.702, 0.031]; // yellow-500
const HEAT_HIGH: Rgb = [0.937, 0.267, 0.267]; // red-500
/** Muted slate for nodes that don't participate in the heat mode. */
export const HEAT_MUTED: Rgb = [0.24, 0.29, 0.36];

function mix(a: Rgb, b: Rgb, t: number): Rgb {
  return [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t];
}

export function heatColor(heat: number): Rgb {
  const t = Math.min(1, Math.max(0, heat));
  return t < 0.5 ? mix(HEAT_LOW, HEAT_MID, t * 2) : mix(HEAT_MID, HEAT_HIGH, (t - 0.5) * 2);
}

// ── Edge kind palette ───────────────────────────────────────────────────────
//
// Their EDGE_TYPE_COLORS, re-keyed onto djinn snapshot edge kinds. Pure kind
// color — endpoint colors are NOT mixed in; per-edge brightness comes from
// the same-cluster/cross-cluster intensity model in GalaxyScene.

const EDGE_KIND_COLORS: Record<string, Rgb> = {
  SymbolReference: hexToRgb(0x1da27e), // their CALLS green
  FileReference: hexToRgb(0x3b82f6), // their IMPORTS blue
  Defines: hexToRgb(0xa855f7), // their DEFINES purple
  ContainsDefinition: hexToRgb(0xa855f7),
  DeclaredInFile: hexToRgb(0xa855f7),
  EntryPointOf: hexToRgb(0xa855f7),
  Extends: hexToRgb(0xf97316), // their IMPLEMENTS orange
  Implements: hexToRgb(0xf97316),
  TraitDispatchCall: hexToRgb(0xa78bfa),
  Writes: hexToRgb(0xe11d48),
  Fetches: hexToRgb(0xeab308), // their HANDLES yellow
  HandlesRoute: hexToRgb(0xeab308),
  Route: hexToRgb(0xeab308),
  StepInProcess: hexToRgb(0x06b6d4),
  CoChangedWith: hexToRgb(0xec4899),
};
const EDGE_DEFAULT: Rgb = hexToRgb(0x1c8585);

export function edgeKindColor(kind: string | undefined): Rgb {
  return (kind && EDGE_KIND_COLORS[kind]) || EDGE_DEFAULT;
}

// ── Density compensation (their density.ts, verbatim constants) ────────────

export const EDGE_REFERENCE_COUNT = 2500;
const EDGE_MIN_SCALE = 0.05;

export function edgeIntensityScale(edgeCount: number): number {
  if (edgeCount <= EDGE_REFERENCE_COUNT) return 1;
  return Math.max(EDGE_MIN_SCALE, Math.sqrt(EDGE_REFERENCE_COUNT / edgeCount));
}

export const NODE_REFERENCE_COUNT = 25_000;
const NODE_FADE_END = 250_000;
const BLOOM_FLOOR = 0.7;
const NODE_BOOST_FLOOR = 0.8;

function fadeFactor(nodeCount: number): number {
  if (nodeCount <= NODE_REFERENCE_COUNT) return 0;
  return Math.min(
    1,
    (nodeCount - NODE_REFERENCE_COUNT) / (NODE_FADE_END - NODE_REFERENCE_COUNT),
  );
}

export function bloomIntensityScale(nodeCount: number): number {
  return 1 - fadeFactor(nodeCount) * (1 - BLOOM_FLOOR);
}

export function nodeBoostScale(nodeCount: number): number {
  return 1 - fadeFactor(nodeCount) * (1 - NODE_BOOST_FLOOR);
}

// ── Channel-dominance glow multiplier (their nodeGlowBoost) ─────────────────
//
// Bloom is luminance-thresholded and blue has a tiny luminance weight, so a
// naive brightness boost blows out white/yellow while blue stays flat.
// Boost by channel dominance instead: blue hubs shine hardest, red leaves
// modestly, white/yellow least. Returns a multiplier ≥ 1.

const GLOW_BASE = 1.35;
const GLOW_BLUE_GAIN = 2.4;
const GLOW_RED_GAIN = 0.9;

export function nodeGlowBoost(r: number, g: number, b: number): number {
  const blueness = Math.max(0, b - Math.max(r, g));
  const redness = Math.max(0, r - Math.max(g, b));
  return GLOW_BASE + blueness * GLOW_BLUE_GAIN + redness * GLOW_RED_GAIN;
}
