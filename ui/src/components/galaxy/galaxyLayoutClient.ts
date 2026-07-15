/**
 * galaxyLayoutClient — run the galaxy force layout off the main thread.
 *
 * Whole-repo graphs (tens of thousands of bodies) take the synchronous
 * `layoutGalaxy` seconds-to-minutes; callers that live in a browser
 * (GalaxyView, the real-data Storybook story) go through this helper so
 * the page never freezes. Falls back to the synchronous path where
 * `Worker` doesn't exist (vitest/jsdom).
 */

import { layoutGalaxy } from "./galaxyLayout";
import type { GalaxyLayoutRequest } from "./galaxyLayout.worker";
import type { GalaxyData, GalaxyNode } from "./galaxyTypes";

export function layoutInWorker(
  nodes: GalaxyNode[],
  edges: GalaxyData["edges"],
  seed: number,
): Promise<GalaxyNode[]> {
  if (typeof Worker === "undefined") {
    layoutGalaxy(nodes, edges, seed);
    return Promise.resolve(nodes);
  }
  return new Promise((resolve, reject) => {
    const worker = new Worker(
      new URL("./galaxyLayout.worker.ts", import.meta.url),
      { type: "module" },
    );
    worker.onmessage = (event: MessageEvent<GalaxyNode[]>) => {
      worker.terminate();
      resolve(event.data);
    };
    worker.onerror = (event) => {
      worker.terminate();
      reject(new Error(event.message || "galaxy layout worker failed"));
    };
    const request: GalaxyLayoutRequest = { nodes, edges, seed };
    worker.postMessage(request);
  });
}
