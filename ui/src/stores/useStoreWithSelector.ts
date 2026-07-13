/**
 * useStoreWithSelector - Base hook for vanilla Zustand stores with selector support
 * 
 * Wraps a vanilla Zustand store (created with subscribeWithSelector)
 * for use in React components. Uses shallow equality by default for
 * selector results to prevent unnecessary re-renders.
 */

import type { StoreApi } from "zustand";
import { useStoreWithEqualityFn } from "zustand/traditional";

// Default shallow equality check
function shallowEqual<T>(a: T, b: T): boolean {
  if (a === b) return true;
  if (typeof a !== typeof b) return false;
  if (typeof a !== "object" || a === null || b === null) return false;

  // Map/Set: use referential equality (already handled by a === b above)
  if (a instanceof Map || a instanceof Set) return false;

  const keysA = Object.keys(a as object);
  const keysB = Object.keys(b as object);

  if (keysA.length !== keysB.length) return false;

  for (const key of keysA) {
    if ((a as Record<string, unknown>)[key] !== (b as Record<string, unknown>)[key]) {
      return false;
    }
  }

  return true;
}

export function useStoreWithSelector<TState, TSelected = TState>(
  store: StoreApi<TState>,
  selector?: (state: TState) => TSelected,
  equalityFn: (a: TSelected, b: TSelected) => boolean = shallowEqual
): TState | TSelected {
  // Delegate the subscription to Zustand's `useStoreWithEqualityFn`, which is
  // built on React's `useSyncExternalStoreWithSelector`. That primitive owns the
  // ref bookkeeping (latest selector / equality fn) in a concurrent-safe way, so
  // this wrapper stays pure and needs no render-phase ref mutation.
  //
  // When no selector is supplied we subscribe to the whole state with the same
  // shallow-equality gate the previous hand-rolled implementation applied.
  const select =
    selector ?? ((state: TState) => state as unknown as TSelected);
  return useStoreWithEqualityFn(store, select, equalityFn);
}
