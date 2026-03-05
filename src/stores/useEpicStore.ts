/**
 * useEpicStore - React hook for epic state with selector support
 * 
 * Uses subscribeWithSelector for granular, performant subscriptions.
 * Components re-render only when their selected slice changes.
 */

import { useCallback } from "react";
import { useStoreWithSelector } from "./useStoreWithSelector";
import { epicStore, type EpicState } from "./epicStore";
import type { Epic } from "../types";

// Re-export the store for direct access
export { epicStore } from "./epicStore";

// Helper hook for subscribing with selectors
export function useEpicStore(): EpicState;
export function useEpicStore<T>(selector: (state: EpicState) => T): T;
export function useEpicStore<T>(selector?: (state: EpicState) => T): EpicState | T {
  return useStoreWithSelector(epicStore, selector);
}

// Convenience hooks for common selections
export function useEpic(id: string): Epic | undefined {
  return useEpicStore(
    useCallback((state) => state.epics.get(id), [id])
  );
}

export function useEpicsByStatus(status: Epic['status']): Epic[] {
  return useEpicStore(
    useCallback((state) => state.getEpicsByStatus(status), [status])
  );
}

export function useAllEpics(): Epic[] {
  return useEpicStore((state) => state.getAllEpics());
}

export function useEpicCount(): number {
  return useEpicStore((state) => state.epics.size);
}
