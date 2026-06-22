import { createContext, useContext } from "react";

/**
 * Maximum container nesting depth. Container blocks (`Tabs`, `Columns`)
 * recursively render their child bodies through the SAME segment→component
 * mapping as the top-level proposal, so a pathological `tabs`-in-`tabs`-in-…
 * body could recurse forever. Once the depth cap is reached a child body is
 * rendered as plain markdown instead of being parsed for further blocks, which
 * terminates the recursion while still showing the authored content.
 */
export const MAX_BLOCK_DEPTH = 4;

/** Current container nesting depth (0 at the top level). */
export const BlockDepthContext = createContext(0);

/** Read the current container nesting depth. */
export function useBlockDepth(): number {
  return useContext(BlockDepthContext);
}
