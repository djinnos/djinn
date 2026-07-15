/**
 * galaxyLayout.worker — runs the synchronous force layout off the main
 * thread. On a whole-repo graph (tens of thousands of bodies) the layout
 * takes seconds; blocking the page for that long is not acceptable, so the
 * live page posts `{ nodes, edges, seed }` here and gets the positioned
 * nodes back. Storybook fixtures keep calling `layoutGalaxy` directly —
 * their module-scope, generate-once model doesn't need a worker.
 */

import { layoutGalaxy } from "./galaxyLayout";
import type { GalaxyEdge, GalaxyNode } from "./galaxyTypes";

export interface GalaxyLayoutRequest {
  nodes: GalaxyNode[];
  edges: GalaxyEdge[];
  seed: number;
}

self.onmessage = (event: MessageEvent<GalaxyLayoutRequest>) => {
  const { nodes, edges, seed } = event.data;
  layoutGalaxy(nodes, edges, seed);
  (self as unknown as Worker).postMessage(nodes);
};
