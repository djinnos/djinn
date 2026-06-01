import { createContext, useContext } from "react";

/**
 * Per-task visual emphasis for the dependency graph, supplied by the
 * Dependencies view and consumed by TaskNode. Kept in context (rather than
 * baked into node data) so emphasis stays current across ELK re-layouts
 * without re-running the layout.
 */
export interface GraphHighlight {
  /** Tasks matching the active search — drawn with a ring, full opacity. */
  highlightTaskIds: Set<string>;
  /** Out-of-scope / non-matching tasks — drawn faded for context. */
  dimTaskIds: Set<string>;
}

const EMPTY: GraphHighlight = {
  highlightTaskIds: new Set(),
  dimTaskIds: new Set(),
};

export const GraphHighlightContext = createContext<GraphHighlight>(EMPTY);

export function useGraphHighlight(): GraphHighlight {
  return useContext(GraphHighlightContext);
}
