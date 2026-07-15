/**
 * galaxyColors — color scales and density compensation for the galaxy view.
 *
 * The constants form one calibrated system — spectral stops, density
 * scales, glow gains, and the Bloom settings in GalaxyScene are tuned
 * against each other, so re-tune them together or not at all. Density
 * compensation + glow-boost approach adapted from the MIT-licensed
 * graph-ui of codebase-memory-mcp.
 */

export type Rgb = [number, number, number];

function hexToRgb(hex: number): Rgb {
  return [((hex >> 16) & 0xff) / 255, ((hex >> 8) & 0xff) / 255, (hex & 0xff) / 255];
}

// ── Heat scale (cognitive-complexity mode) ──────────────────────────────────

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

// ── Group (crate/community) colors ──────────────────────────────────────────
//
// Generated, not palette-indexed: a fixed 12-hue palette collides badly
// past a dozen groups (60 crates → ~5 crates per color). Instead the hue
// comes from the group-name hash (continuous, so 100+ communities spread
// over the whole wheel) while saturation/lightness stay pinned to the
// Tailwind-500 band the rest of the UI speaks — dynamic hues, consistent
// voice. Hash-derived means a crate keeps its color everywhere, always,
// regardless of which other crates are visible.

function fnv1a(input: string): number {
  let h = 2166136261;
  for (let i = 0; i < input.length; i++) {
    h ^= input.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return h >>> 0;
}

function hslToRgb(h: number, s: number, l: number): Rgb {
  const chroma = (1 - Math.abs(2 * l - 1)) * s;
  const hp = (((h % 360) + 360) % 360) / 60;
  const x = chroma * (1 - Math.abs((hp % 2) - 1));
  let r = 0;
  let g = 0;
  let b = 0;
  if (hp < 1) [r, g, b] = [chroma, x, 0];
  else if (hp < 2) [r, g, b] = [x, chroma, 0];
  else if (hp < 3) [r, g, b] = [0, chroma, x];
  else if (hp < 4) [r, g, b] = [0, x, chroma];
  else if (hp < 5) [r, g, b] = [x, 0, chroma];
  else [r, g, b] = [chroma, 0, x];
  const m = l - chroma / 2;
  return [r + m, g + m, b + m];
}

function groupHsl(group: string): [number, number, number] {
  const h = fnv1a(group);
  const hue = (h & 0xffff) / 65535 * 360;
  // Small hash-driven wiggle inside the Tailwind-500 band so near-hue
  // groups still separate slightly, without leaving the family.
  const saturation = 0.72 + (((h >> 16) & 0xff) / 255) * 0.14; // 0.72–0.86
  const lightness = 0.55 + (((h >> 24) & 0xff) / 255) * 0.08; // 0.55–0.63
  return [hue, saturation, lightness];
}

export function groupColor(group: string | undefined): Rgb {
  if (!group) return HEAT_MUTED;
  const [h, s, l] = groupHsl(group);
  return hslToRgb(h, s, l);
}

/** CSS color for the group dots/legend chips (same hash, same color). */
export function groupColorCss(group: string): string {
  const [h, s, l] = groupHsl(group);
  return `hsl(${h.toFixed(1)} ${(s * 100).toFixed(0)}% ${(l * 100).toFixed(0)}%)`;
}

// ── Edge kind palette ───────────────────────────────────────────────────────
//
// Pure kind color — endpoint colors are NOT mixed in; per-edge brightness
// comes from the same-cluster/cross-cluster intensity model in GalaxyScene.

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

// ── Density compensation ────────────────────────────────────────────────────
//
// Edges blend additively, so glow is linear in edge count — without
// compensation a big graph saturates into a white blob. Edge intensity
// scales ~1/sqrt(n) past the reference; node glow and bloom ease toward
// floors only on very large clouds.

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

// ── Channel-dominance glow multiplier ───────────────────────────────────────
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
