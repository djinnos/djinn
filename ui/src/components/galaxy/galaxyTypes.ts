/**
 * galaxyTypes — the renderer-facing data model for the 3D "galaxy" view.
 *
 * Deliberately generic: nothing here knows about SCIP, symbols, or memory
 * notes. The code graph feeds it through an adapter (positions from the
 * warm-time server layout, degree from edge counts); the memory graph can
 * feed the same shape. Keeping the model this small is what lets one
 * renderer serve both.
 */

export interface GalaxyNode {
  id: string;
  label: string;
  x: number;
  y: number;
  z: number;
  /** Total degree (in + out) — drives the stellar color scale. */
  degree: number;
  /** Visual radius basis in world units (roughly 3–14). */
  size: number;
  /**
   * Grouping key (top-level directory / crate / memory scope). Drives
   * cluster placement in `layoutGalaxy` and the group fly-to affordance.
   */
  group?: string;
  /**
   * Optional anchor node id (a symbol's file, a memory's scope). Parented
   * nodes are laid out in a tight clump around their anchor, giving
   * clusters their grape-bunch texture instead of a uniform cloud.
   */
  parent?: string;
  /** Test file/symbol — drives the hide-tests toggle. */
  isTest?: boolean;
  /** Workspace slug (multi-workspace projects) — drives the workspace filter. */
  workspace?: string;
}

export interface GalaxyEdge {
  source: string;
  target: string;
  /** Relationship kind — mapped to a color channel; unknown kinds get the default. */
  kind?: string;
}

export interface GalaxyData {
  nodes: GalaxyNode[];
  edges: GalaxyEdge[];
  /** Full pre-budget totals for the "showing N of M" honesty line. */
  totalNodes?: number;
  totalEdges?: number;
  /**
   * Proposal lmkv: true when every node's x/y/z came straight from the server's
   * warm-time galaxy layout (shipped in the snapshot), so the caller can render
   * as-is and MUST NOT run the client force layout. False/undefined means
   * positions still need the worker/inline layout (legacy blobs, fixtures).
   */
  serverPositioned?: boolean;
}

/**
 * User display multipliers layered on top of the automatic density
 * compensation. 1.0 means "follow auto density".
 */
export interface GalaxyDisplay {
  edgeBrightness: number;
  nodeGlow: number;
  bloom: number;
}

export const DEFAULT_GALAXY_DISPLAY: GalaxyDisplay = {
  edgeBrightness: 1,
  nodeGlow: 1,
  bloom: 1,
};
