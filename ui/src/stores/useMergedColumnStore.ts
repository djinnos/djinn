/**
 * useMergedColumnStore - React hook for Merged-column pagination state.
 *
 * Uses subscribeWithSelector for granular, performant subscriptions so the
 * Kanban's "Load more" affordance re-renders only when its slice changes.
 */

import { useStoreWithSelector } from "./useStoreWithSelector";
import { mergedColumnStore, type MergedColumnState } from "./mergedColumnStore";

export { mergedColumnStore } from "./mergedColumnStore";

export function useMergedColumnStore(): MergedColumnState;
export function useMergedColumnStore<T>(
  selector: (state: MergedColumnState) => T,
): T;
export function useMergedColumnStore<T>(
  selector?: (state: MergedColumnState) => T,
): MergedColumnState | T {
  return useStoreWithSelector(mergedColumnStore, selector);
}
