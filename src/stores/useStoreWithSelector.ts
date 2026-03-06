/**
 * useStoreWithSelector - Base hook for vanilla Zustand stores with selector support
 * 
 * Wraps a vanilla Zustand store (created with subscribeWithSelector)
 * for use in React components. Uses shallow equality by default for
 * selector results to prevent unnecessary re-renders.
 */

import { useEffect, useState, useRef } from "react";
import type { StoreApi } from "zustand";

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
  const selectorRef = useRef(selector);
  selectorRef.current = selector;
  
  const equalityFnRef = useRef(equalityFn);
  equalityFnRef.current = equalityFn;
  
  // Get initial state
  const [selectedState, setSelectedState] = useState<TSelected>(() => {
    const state = store.getState();
    return selectorRef.current ? selectorRef.current(state) : (state as unknown as TSelected);
  });
  
  // Subscribe to store changes
  useEffect(() => {
    const unsubscribe = store.subscribe(
      (state) => {
        const selected = selectorRef.current ? selectorRef.current(state) : (state as unknown as TSelected);
        setSelectedState((prev) => {
          if (equalityFnRef.current(prev, selected)) {
            return prev;
          }
          return selected;
        });
      }
    );
    
    return unsubscribe;
  }, [store]);
  
  // Return full state if no selector, otherwise selected slice
  return selector ? selectedState : (store.getState() as TState | TSelected);
}
