/**
 * useBoardSearchStore - React hook for the board search in-flight state.
 */

import { useStoreWithSelector } from "./useStoreWithSelector";
import { boardSearchStore, type BoardSearchState } from "./boardSearchStore";

export { boardSearchStore } from "./boardSearchStore";

export function useBoardSearchStore(): BoardSearchState;
export function useBoardSearchStore<T>(
  selector: (state: BoardSearchState) => T,
): T;
export function useBoardSearchStore<T>(
  selector?: (state: BoardSearchState) => T,
): BoardSearchState | T {
  return useStoreWithSelector(boardSearchStore, selector);
}
