/**
 * galaxyLayout — deterministic 3D layout for galaxy data.
 *
 * A TypeScript port of codebase-memory-mcp's `src/ui/layout3d.c` (MIT,
 * © 2025 DeusData), which is the algorithm behind their "code galaxy":
 *
 *   1. Seed each node on a ring position hashed from its cluster key
 *      (djinn: the `group` — crate/top-level dir), with small jitter, and
 *      z from call depth (entry points at top, callees below).
 *   2. Gentle ForceAtlas2-style local optimization: Barnes-Hut octree
 *      repulsion + linear edge attraction + an anchor spring back to the
 *      seed position, displacement-capped per iteration.
 *
 * The cluster blobs, trunk bundles, and globe silhouette all EMERGE from
 * this pass — same-group nodes start coincident and repel into blobs,
 * edges pull related blobs together, anchors keep the ring silhouette.
 *
 * This is the client-side stand-in for the warm-time server layout
 * (proposal lmkv ships the same algorithm in Rust and puts x/y/z in the
 * snapshot payload); in Storybook it runs once per fixture at build time.
 */

import type { GalaxyEdge, GalaxyNode } from "./galaxyTypes";

// Their layout3d.c constants, verbatim.
const LOCAL_REPULSION = 8.0;
const LOCAL_ATTRACTION = 1.0;
const LOCAL_ANCHOR_K = 0.25;
const LOCAL_ITERATIONS = 40;
const Z_DEPTH_SPACING = 50.0;
const BH_THETA = 1.2;
const OCTREE_MAX_DEPTH = 20;
const OCTREE_MIN_HALF = 1e-4;
const MAX_DISPLACEMENT = 8.0;
const RING_BASE_RADIUS = 500.0;
const RING_RADIUS_SPREAD = 250.0;
const SEED_JITTER = 40.0;

/** mulberry32 — tiny seeded PRNG, deterministic across runs. */
export function mulberry32(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a |= 0;
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function fnv1a(input: string): number {
  let h = 2166136261;
  for (let i = 0; i < input.length; i++) {
    h ^= input.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return h >>> 0;
}

// ── Call depth via BFS (their compute_call_depth, generalized) ──────────────
//
// Entry points = nodes with outgoing edges but no incoming ones (their C
// uses is_entry_point flags; a degree-based definition is the generic
// equivalent). Unreached nodes settle at depth 0.

function computeDepths(
  nodes: GalaxyNode[],
  edges: GalaxyEdge[],
  indexById: Map<string, number>,
): Int32Array {
  const n = nodes.length;
  const depth = new Int32Array(n).fill(-1);
  const inDegree = new Int32Array(n);
  const outAdjacency: number[][] = Array.from({ length: n }, () => []);
  for (const e of edges) {
    const s = indexById.get(e.source);
    const t = indexById.get(e.target);
    if (s === undefined || t === undefined) continue;
    outAdjacency[s].push(t);
    inDegree[t]++;
  }

  const queue: number[] = [];
  for (let i = 0; i < n; i++) {
    if (inDegree[i] === 0 && outAdjacency[i].length > 0) {
      depth[i] = 0;
      queue.push(i);
    }
  }
  let head = 0;
  while (head < queue.length) {
    const c = queue[head++];
    for (const t of outAdjacency[c]) {
      if (depth[t] === -1) {
        depth[t] = depth[c] + 1;
        queue.push(t);
      }
    }
  }
  for (let i = 0; i < n; i++) if (depth[i] === -1) depth[i] = 0;
  return depth;
}

// ── Barnes-Hut octree (faithful port of their octree_*) ─────────────────────
//
// Each leaf holds exactly ONE body (identified by index, so a query can
// exclude itself); inserting into an occupied leaf splits it and re-inserts
// the resident body. Getting this right matters more than it looks: an
// aggregates-only octree under-counts near-field repulsion in dense
// clusters, attraction wins, and the whole galaxy collapses into a bar.

interface Octree {
  cx: number[];
  cy: number[];
  cz: number[];
  half: number[];
  mass: number[];
  mx: number[];
  my: number[];
  mz: number[];
  /** First child cell index, -1 = leaf. */
  child: number[];
  /** Resident body index for occupied leaves, -1 otherwise. */
  body: number[];
  bx: number[];
  by: number[];
  bz: number[];
  bmass: number[];
}

function octreeNew(cx: number, cy: number, cz: number, half: number): Octree {
  return {
    cx: [cx], cy: [cy], cz: [cz], half: [half],
    mass: [0], mx: [0], my: [0], mz: [0],
    child: [-1], body: [-1],
    bx: [0], by: [0], bz: [0], bmass: [0],
  };
}

function octreeAlloc(o: Octree, cx: number, cy: number, cz: number, half: number): number {
  o.cx.push(cx); o.cy.push(cy); o.cz.push(cz); o.half.push(half);
  o.mass.push(0); o.mx.push(0); o.my.push(0); o.mz.push(0);
  o.child.push(-1); o.body.push(-1);
  o.bx.push(0); o.by.push(0); o.bz.push(0); o.bmass.push(0);
  return o.cx.length - 1;
}

function octantOf(o: Octree, cell: number, x: number, y: number, z: number): number {
  return (x >= o.cx[cell] ? 1 : 0) | (y >= o.cy[cell] ? 2 : 0) | (z >= o.cz[cell] ? 4 : 0);
}

function octreeSplit(o: Octree, cell: number): void {
  const h = o.half[cell] * 0.5;
  const base = octreeAlloc(o, o.cx[cell] - h, o.cy[cell] - h, o.cz[cell] - h, h);
  for (let i = 1; i < 8; i++) {
    octreeAlloc(
      o,
      o.cx[cell] + ((i & 1) ? h : -h),
      o.cy[cell] + ((i & 2) ? h : -h),
      o.cz[cell] + ((i & 4) ? h : -h),
      h,
    );
  }
  o.child[cell] = base;
}

function octreeInsert(
  o: Octree,
  bodyIndex: number,
  x: number,
  y: number,
  z: number,
  mass: number,
): void {
  let cell = 0;
  let depth = 0;
  for (;;) {
    o.mass[cell] += mass;
    o.mx[cell] += x * mass;
    o.my[cell] += y * mass;
    o.mz[cell] += z * mass;

    if (o.child[cell] === -1) {
      if (o.body[cell] === -1) {
        // Empty leaf — place the body here.
        o.body[cell] = bodyIndex;
        o.bx[cell] = x;
        o.by[cell] = y;
        o.bz[cell] = z;
        o.bmass[cell] = mass;
        return;
      }
      // Occupied leaf. At the depth/size floor, fold into the aggregate
      // (coincident-point OOM guard — rare thanks to seed jitter).
      if (depth >= OCTREE_MAX_DEPTH || o.half[cell] < OCTREE_MIN_HALF) return;

      // Split and push the resident body down one level.
      const rb = o.body[cell];
      const rx = o.bx[cell];
      const ry = o.by[cell];
      const rz = o.bz[cell];
      const rm = o.bmass[cell];
      octreeSplit(o, cell);
      o.body[cell] = -1;
      const rCell = o.child[cell] + octantOf(o, cell, rx, ry, rz);
      o.mass[rCell] += rm;
      o.mx[rCell] += rx * rm;
      o.my[rCell] += ry * rm;
      o.mz[rCell] += rz * rm;
      o.body[rCell] = rb;
      o.bx[rCell] = rx;
      o.by[rCell] = ry;
      o.bz[rCell] = rz;
      o.bmass[rCell] = rm;
      // Fall through: descend for the incoming body.
    }
    cell = o.child[cell] + octantOf(o, cell, x, y, z);
    depth++;
  }
}

function octreeRepulse(
  o: Octree,
  selfIndex: number,
  x: number,
  y: number,
  z: number,
  mass: number,
  repulsion: number,
  force: { fx: number; fy: number; fz: number },
): void {
  const stack = [0];
  while (stack.length > 0) {
    const cell = stack.pop()!;
    const m = o.mass[cell];
    if (m <= 0) continue;

    const isLeaf = o.child[cell] === -1;
    if (isLeaf && o.body[cell] === selfIndex) continue; // self

    const comx = o.mx[cell] / m;
    const comy = o.my[cell] / m;
    const comz = o.mz[cell] / m;
    const dx = x - comx;
    const dy = y - comy;
    const dz = z - comz;
    const d = Math.sqrt(dx * dx + dy * dy + dz * dz);

    if (isLeaf || (o.half[cell] * 2) / (d + 0.001) < BH_THETA) {
      if (d < 0.001) continue; // coincident — no defined direction
      const f = (repulsion * mass * m) / (d * d + 0.01);
      force.fx += (dx / d) * f;
      force.fy += (dy / d) * f;
      force.fz += (dz / d) * f;
    } else {
      const base = o.child[cell];
      for (let i = 0; i < 8; i++) stack.push(base + i);
    }
  }
}

// ── Layout entry point ──────────────────────────────────────────────────────

/**
 * Positions `nodes` in place. Deterministic for a given (data, seed).
 * Iteration count scales down with size, mirroring their C (which drops
 * from 40 → 20 → 10 as graphs grow; JS pays ~an order of magnitude more
 * per iteration, so the thresholds sit lower).
 */
export function layoutGalaxy(
  nodes: GalaxyNode[],
  edges: GalaxyEdge[] = [],
  seed = 1337,
): void {
  const n = nodes.length;
  if (n === 0) return;

  const indexById = new Map<string, number>();
  nodes.forEach((node, i) => indexById.set(node.id, i));
  const depths = computeDepths(nodes, edges, indexById);

  // 1. Seed: ring position hashed from the cluster key + jitter, z by depth.
  const x = new Float32Array(n);
  const y = new Float32Array(n);
  const z = new Float32Array(n);
  const ax = new Float32Array(n);
  const ay = new Float32Array(n);
  const az = new Float32Array(n);
  const mass = new Float32Array(n);

  for (let i = 0; i < n; i++) {
    const node = nodes[i];
    const clusterKey = node.group ?? "";
    const h = fnv1a(clusterKey);
    // Departure from the C original: cluster centers sit on a hashed
    // SPHERE, not a 2D ring — the ring + depth-only-z reads as a flat
    // pancake once you orbit it. Same emergent behavior (same-group
    // nodes coincide, the force pass inflates them into blobs), true
    // globe silhouette.
    const theta = ((h & 0xffff) / 65535) * Math.PI * 2;
    const phi = Math.acos(2 * (((h >> 16) & 0xff) / 255) - 1);
    const radius = RING_BASE_RADIUS + (((h >> 24) & 0xff) / 255) * RING_RADIUS_SPREAD;

    const rng = mulberry32(fnv1a(node.id) ^ seed);
    const px = radius * Math.sin(phi) * Math.cos(theta) + (rng() * 2 - 1) * SEED_JITTER;
    const py = radius * Math.sin(phi) * Math.sin(theta) + (rng() * 2 - 1) * SEED_JITTER;
    const pz =
      radius * Math.cos(phi) +
      (rng() * 2 - 1) * SEED_JITTER -
      depths[i] * Z_DEPTH_SPACING;

    x[i] = ax[i] = px;
    y[i] = ay[i] = py;
    z[i] = az[i] = pz;
    mass[i] = node.degree + 1;
  }

  // Edge index pairs (dropping unresolvable endpoints once, not per iter).
  const pairCount = edges.length;
  const es = new Int32Array(pairCount);
  const ed = new Int32Array(pairCount);
  let pairs = 0;
  for (const e of edges) {
    const s = indexById.get(e.source);
    const t = indexById.get(e.target);
    if (s === undefined || t === undefined) continue;
    es[pairs] = s;
    ed[pairs] = t;
    pairs++;
  }

  // 2. Local optimization.
  let iterations = LOCAL_ITERATIONS;
  if (n > 60_000) iterations = 12;
  else if (n > 15_000) iterations = 24;

  const force = { fx: 0, fy: 0, fz: 0 };
  const fx = new Float32Array(n);
  const fy = new Float32Array(n);
  const fz = new Float32Array(n);

  for (let iter = 0; iter < iterations; iter++) {
    fx.fill(0);
    fy.fill(0);
    fz.fill(0);

    // Bounding box → octree root.
    let mnx = Infinity, mny = Infinity, mnz = Infinity;
    let mxx = -Infinity, mxy = -Infinity, mxz = -Infinity;
    for (let i = 0; i < n; i++) {
      if (x[i] < mnx) mnx = x[i];
      if (y[i] < mny) mny = y[i];
      if (z[i] < mnz) mnz = z[i];
      if (x[i] > mxx) mxx = x[i];
      if (y[i] > mxy) mxy = y[i];
      if (z[i] > mxz) mxz = z[i];
    }
    const half = Math.max(mxx - mnx, mxy - mny, mxz - mnz) * 0.5 + 1;
    const tree = octreeNew((mnx + mxx) * 0.5, (mny + mxy) * 0.5, (mnz + mxz) * 0.5, half);
    for (let i = 0; i < n; i++) octreeInsert(tree, i, x[i], y[i], z[i], mass[i]);

    // Repulsion (self excluded by body index, as in the C original).
    for (let i = 0; i < n; i++) {
      force.fx = 0;
      force.fy = 0;
      force.fz = 0;
      octreeRepulse(tree, i, x[i], y[i], z[i], mass[i], LOCAL_REPULSION, force);
      fx[i] += force.fx;
      fy[i] += force.fy;
      fz[i] += force.fz;
    }

    // Attraction along edges (linear spring).
    for (let e = 0; e < pairs; e++) {
      const s = es[e];
      const t = ed[e];
      const dx = x[t] - x[s];
      const dy = y[t] - y[s];
      const dz = z[t] - z[s];
      fx[s] += dx * LOCAL_ATTRACTION;
      fy[s] += dy * LOCAL_ATTRACTION;
      fz[s] += dz * LOCAL_ATTRACTION;
      fx[t] -= dx * LOCAL_ATTRACTION;
      fy[t] -= dy * LOCAL_ATTRACTION;
      fz[t] -= dz * LOCAL_ATTRACTION;
    }

    // Anchor spring back to the ring seed.
    for (let i = 0; i < n; i++) {
      fx[i] += (ax[i] - x[i]) * LOCAL_ANCHOR_K * mass[i];
      fy[i] += (ay[i] - y[i]) * LOCAL_ANCHOR_K * mass[i];
      fz[i] += (az[i] - z[i]) * LOCAL_ANCHOR_K * mass[i];
    }

    // Apply with capped displacement.
    for (let i = 0; i < n; i++) {
      const fm = Math.sqrt(fx[i] * fx[i] + fy[i] * fy[i] + fz[i] * fz[i]);
      let speed = 1.0;
      if (speed * fm > MAX_DISPLACEMENT) speed = MAX_DISPLACEMENT / (fm + 0.001);
      x[i] += fx[i] * speed;
      y[i] += fy[i] * speed;
      z[i] += fz[i] * speed;
    }
  }

  for (let i = 0; i < n; i++) {
    nodes[i].x = x[i];
    nodes[i].y = y[i];
    nodes[i].z = z[i];
  }
}

/** Centroid + spread of a node set — feeds the camera fly-to. */
export function boundsOf(nodes: GalaxyNode[]): {
  center: [number, number, number];
  radius: number;
} {
  if (nodes.length === 0) return { center: [0, 0, 0], radius: 400 };
  let sx = 0;
  let sy = 0;
  let sz = 0;
  for (const n of nodes) {
    sx += n.x;
    sy += n.y;
    sz += n.z;
  }
  const cx = sx / nodes.length;
  const cy = sy / nodes.length;
  const cz = sz / nodes.length;
  let maxSq = 0;
  for (const n of nodes) {
    const dx = n.x - cx;
    const dy = n.y - cy;
    const dz = n.z - cz;
    maxSq = Math.max(maxSq, dx * dx + dy * dy + dz * dz);
  }
  return { center: [cx, cy, cz], radius: Math.max(60, Math.sqrt(maxSq)) };
}
