/**
 * galaxyFixture — seeded procedural "codebase" generator for Storybook.
 *
 * Produces graphs shaped like real repositories so the galaxy can be judged
 * at realistic scale without a live server: packages containing files
 * containing symbols, preferential-attachment call edges (power-law degree
 * distribution → a few blazing hubs, many dim leaves), and cross-package
 * edges that render as the long strands between clusters.
 */

import { layoutGalaxy, mulberry32 } from "./galaxyLayout";
import type { GalaxyData, GalaxyEdge, GalaxyNode } from "./galaxyTypes";

const PACKAGE_NAMES = [
  "server", "ui", "agent", "db", "graph", "memory", "k8s", "mcp",
  "auth", "billing", "control-plane", "worker", "indexer", "runtime",
  "supervisor", "git", "stack", "image-builder", "extension", "chat",
  "proposals", "tasks", "sessions", "telemetry", "search", "embeddings",
  "routes", "scheduler", "quota", "webhooks", "notifications", "exports",
  "audit", "policies", "secrets", "migrations", "fixtures", "tooling",
  "cli", "sdk", "types", "utils", "config", "metrics", "tracing",
  "cache", "queue", "storage",
];

const FILE_STEMS = [
  "handler", "service", "store", "types", "config", "client", "router",
  "parser", "builder", "runner", "watcher", "codec", "guard", "policy",
  "adapter", "bridge", "registry", "pool", "session", "worker",
];

const SYMBOL_STEMS = [
  "resolve", "dispatch", "build", "parse", "validate", "apply", "merge",
  "load", "persist", "sync", "hydrate", "collect", "emit", "route",
  "authorize", "encode", "decode", "spawn", "reconcile", "observe",
];

export interface GalaxyFixtureOptions {
  seed?: number;
  packages: number;
  filesPerPackage: number;
  /** Mean symbols per file (actual counts are power-law-ish, 1..~14). */
  symbolsPerFile: number;
}

export function makeGalaxyFixture(options: GalaxyFixtureOptions): GalaxyData {
  const { seed = 42, packages, filesPerPackage, symbolsPerFile } = options;
  const rng = mulberry32(seed);

  const nodes: GalaxyNode[] = [];
  const edges: GalaxyEdge[] = [];
  /** Per-package symbol ids, for intra-package wiring. */
  const packageSymbols: string[][] = [];
  /** Globally attractive hubs — targets for cross-package edges. */
  const hubIds: string[] = [];

  for (let p = 0; p < packages; p++) {
    const pkg = `${PACKAGE_NAMES[p % PACKAGE_NAMES.length]}${p >= PACKAGE_NAMES.length ? `-${Math.floor(p / PACKAGE_NAMES.length)}` : ""}`;
    const symbols: string[] = [];
    packageSymbols.push(symbols);

    const fileCount = Math.max(2, Math.round(filesPerPackage * (0.5 + rng())));
    for (let f = 0; f < fileCount; f++) {
      const stem = FILE_STEMS[Math.floor(rng() * FILE_STEMS.length)];
      const fileId = `${pkg}/file:${stem}-${f}`;
      nodes.push({
        id: fileId,
        label: `${stem}_${f}.rs`,
        x: 0, y: 0, z: 0,
        degree: 0,
        size: 8,
        group: pkg,
      });

      // Power-law-ish symbol count: most files small, a few big.
      const symCount = Math.max(1, Math.round(symbolsPerFile * Math.pow(rng(), 1.6) * 2.4));
      let prevSym: string | null = null;
      for (let s = 0; s < symCount; s++) {
        const stem2 = SYMBOL_STEMS[Math.floor(rng() * SYMBOL_STEMS.length)];
        const isType = rng() < 0.16;
        const symId = `${fileId}#${stem2}${s}`;
        nodes.push({
          id: symId,
          label: isType ? `${capitalize(stem2)}${s}` : `${stem2}_${s}`,
          x: 0, y: 0, z: 0,
          degree: 0,
          size: isType ? 6 : 4,
          group: pkg,
          parent: fileId,
        });
        symbols.push(symId);
        edges.push({ source: fileId, target: symId, kind: "Defines" });
        // Local chain: neighbors in a file often reference each other.
        if (prevSym && rng() < 0.38) {
          edges.push({ source: symId, target: prevSym, kind: "SymbolReference" });
        }
        prevSym = symId;
        if (rng() < 0.03) hubIds.push(symId);
      }

      // Leaf fields/consts: defined, never referenced → the red dwarf rim.
      const fieldCount = Math.floor(rng() * 4);
      for (let leafIndex = 0; leafIndex < fieldCount; leafIndex++) {
        const fieldId = `${fileId}@field${leafIndex}`;
        nodes.push({
          id: fieldId,
          label: `FIELD_${leafIndex}`,
          x: 0, y: 0, z: 0,
          degree: 0,
          size: 4,
          group: pkg,
          parent: fileId,
        });
        edges.push({ source: fileId, target: fieldId, kind: "Defines" });
      }
    }

    // Intra-package wiring with preferential attachment: reuse of earlier
    // targets concentrates degree into hubs.
    const intraEdges = Math.round(symbols.length * 1.15);
    const recentTargets: string[] = [];
    for (let e = 0; e < intraEdges && symbols.length > 2; e++) {
      const source = symbols[Math.floor(rng() * symbols.length)];
      const preferential = recentTargets.length > 4 && rng() < 0.6;
      const target = preferential
        ? recentTargets[Math.floor(rng() * recentTargets.length)]
        : symbols[Math.floor(rng() * symbols.length)];
      if (source === target) continue;
      const kindRoll = rng();
      const kind = kindRoll < 0.55 ? "SymbolReference" : kindRoll < 0.8 ? "Reads" : kindRoll < 0.9 ? "Writes" : "Implements";
      edges.push({ source, target, kind });
      recentTargets.push(target);
      if (recentTargets.length > 24) recentTargets.shift();
    }
  }

  // Cross-package edges concentrate on "hot pairs" — real repos have a few
  // dominant dependency directions, and many roughly-parallel edges between
  // the same two clusters is exactly what renders as the bundled trunks.
  const allSymbols = packageSymbols.flat();
  const hotPairCount = Math.max(3, Math.round(packages * 1.4));
  for (let pair = 0; pair < hotPairCount && packages > 1; pair++) {
    const a = Math.floor(rng() * packages);
    let b = Math.floor(rng() * packages);
    if (a === b) b = (b + 1) % packages;
    const from = packageSymbols[a];
    const to = packageSymbols[b];
    if (from.length === 0 || to.length === 0) continue;
    const trunkWidth = 4 + Math.floor(rng() * rng() * 26);
    for (let e = 0; e < trunkWidth; e++) {
      edges.push({
        source: from[Math.floor(rng() * from.length)],
        target: to[Math.floor(rng() * to.length)],
        kind: rng() < 0.9 ? "SymbolReference" : "Fetches",
      });
    }
  }
  // Plus scattered hub pulls so blue giants collect cross-cluster fan-in.
  const scatterEdges = Math.round(nodes.length * 0.05);
  for (let e = 0; e < scatterEdges && hubIds.length > 0 && allSymbols.length > 0; e++) {
    const source = allSymbols[Math.floor(rng() * allSymbols.length)];
    const target = hubIds[Math.floor(rng() * hubIds.length)];
    if (source === target) continue;
    edges.push({ source, target, kind: "SymbolReference" });
  }

  // Materialize degrees, then grow hub sizes logarithmically.
  const degreeById = new Map<string, number>();
  for (const edge of edges) {
    degreeById.set(edge.source, (degreeById.get(edge.source) ?? 0) + 1);
    degreeById.set(edge.target, (degreeById.get(edge.target) ?? 0) + 1);
  }
  for (const node of nodes) {
    node.degree = degreeById.get(node.id) ?? 0;
    if (node.degree > 5) node.size += Math.min(node.degree * 0.3, 10);
  }

  layoutGalaxy(nodes, edges, seed);
  return { nodes, edges, totalNodes: nodes.length, totalEdges: edges.length };
}

function capitalize(s: string): string {
  return s.charAt(0).toUpperCase() + s.slice(1);
}

// ── Storybook presets ───────────────────────────────────────────────────────

export const FIXTURE_PRESETS = {
  small: { seed: 7, packages: 8, filesPerPackage: 10, symbolsPerFile: 3 },
  medium: { seed: 11, packages: 26, filesPerPackage: 22, symbolsPerFile: 4 },
  large: { seed: 23, packages: 48, filesPerPackage: 46, symbolsPerFile: 5 },
  /** ~50k nodes — the uncapped-snapshot regime the renderer must survive. */
  xlarge: { seed: 31, packages: 80, filesPerPackage: 108, symbolsPerFile: 6 },
} satisfies Record<string, GalaxyFixtureOptions>;
