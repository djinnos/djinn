import type { ComponentType } from "react";

import type { BlockProps } from "@/components/proposals/blocks/types";
import {
  BLOCK_COMPONENTS,
  BLOCK_DISPLAY_NAMES,
  PROPOSAL_BLOCK_SPECS,
  type ProposalBlockDefinition,
  type ProposalBlockSpec,
} from "@/components/proposals/blocks/registry";

// Re-export the derived component/display-name maps so consumers that key off
// the canonical MDX tag keep their original public surface. Both are DERIVED
// from the single `defineBlock()` source in
// `@/components/proposals/blocks/registry` — they are never hand-maintained.
export { BLOCK_COMPONENTS, BLOCK_DISPLAY_NAMES };

export interface BlockTypeDefinition extends ProposalBlockDefinition {
  displayName: string;
  requiredFields: string[];
  component: ComponentType<BlockProps>;
}

function toBlockTypeDefinition(spec: ProposalBlockSpec): BlockTypeDefinition {
  return {
    type: spec.type,
    tag: spec.tag,
    fields: spec.fields,
    displayName: spec.displayName,
    requiredFields: ["id"],
    component: spec.component,
  };
}

// `BLOCK_TYPES` / the by-tag map are built LAZILY on first access. Block
// component modules (e.g. `ColumnsBlock` → `ProposalBlocks`) import
// `getBlockByTag` from this module, which creates an import cycle back to
// `registry`; deferring the read until first call ensures `PROPOSAL_BLOCK_SPECS`
// is fully initialized before we touch it, instead of reading a TDZ/undefined
// value at module-init time.
let blockTypesCache: BlockTypeDefinition[] | null = null;
let blockTypesByTagCache: Map<string, BlockTypeDefinition> | null = null;

/**
 * Render registry derived from the canonical proposal block contract.
 *
 * Each block is defined ONCE as a `defineBlock()` spec; this adapts those specs
 * to the `BlockTypeDefinition` shape (adding the universally required `id`
 * attribute). The MDX tag names, components, and display names all come from
 * that single source so this file cannot drift into a second, contradictory
 * registry. All blocks require an `id` attribute so feedback comments and the
 * debate trail can anchor to a specific block across proposal revisions.
 */
export function getBlockTypes(): BlockTypeDefinition[] {
  if (!blockTypesCache) {
    blockTypesCache = PROPOSAL_BLOCK_SPECS.map(toBlockTypeDefinition);
  }
  return blockTypesCache;
}

/**
 * The list of block type definitions. Backed by a lazy getter to stay
 * cycle-safe; the value is identical to calling {@link getBlockTypes}.
 */
export const BLOCK_TYPES: BlockTypeDefinition[] = new Proxy(
  [] as BlockTypeDefinition[],
  {
    get(_target, prop, receiver) {
      return Reflect.get(getBlockTypes(), prop, receiver);
    },
    has(_target, prop) {
      return Reflect.has(getBlockTypes(), prop);
    },
    ownKeys() {
      return Reflect.ownKeys(getBlockTypes());
    },
    getOwnPropertyDescriptor(_target, prop) {
      return Reflect.getOwnPropertyDescriptor(getBlockTypes(), prop);
    },
  },
);

/**
 * Look up a block type definition by its canonical PascalCase MDX tag name.
 *
 * Returns `undefined` when no registered block type matches the given tag.
 */
export function getBlockByTag(tag: string): BlockTypeDefinition | undefined {
  if (!blockTypesByTagCache) {
    blockTypesByTagCache = new Map(
      getBlockTypes().map((block) => [block.tag, block]),
    );
  }
  return blockTypesByTagCache.get(tag);
}
