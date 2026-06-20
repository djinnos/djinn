import {
  PROPOSAL_BLOCK_REGISTRY,
  getProposalBlockDefinition,
  getProposalBlockDefinitionByTag,
  type ProposalBlockDefinition,
  type ProposalBlockFieldMap,
  type ProposalBlockFieldSchema,
  type ProposalBlockRegistry,
} from "@/lib/proposalBlocks";

import { parseMdxBody } from "./parseMdxBody";

export {
  PROPOSAL_BLOCK_REGISTRY,
  getProposalBlockDefinition,
  getProposalBlockDefinitionByTag,
};
export type {
  ProposalBlockDefinition,
  ProposalBlockFieldMap,
  ProposalBlockFieldSchema,
  ProposalBlockRegistry,
};

export interface ExtractedProposalBlockId {
  /** Stable id attribute used for deep-link anchors and feedback targeting. */
  id: string;
  /** Canonical PascalCase MDX component tag, e.g. `DataModel`. */
  tag: string;
  /** Stable registry block type, e.g. `data-model`. */
  type: string;
}

/**
 * TypeScript mirror of the Rust proposal block registry for UI-side validation
 * and rendering. The registry maps stable block type → MDX tag → field schema.
 */
export const PROPOSAL_BLOCK_DEFINITIONS_BY_TYPE = PROPOSAL_BLOCK_REGISTRY;

export const PROPOSAL_BLOCK_DEFINITIONS_BY_TAG = Object.fromEntries(
  Object.values(PROPOSAL_BLOCK_REGISTRY).map((definition) => [
    definition.tag,
    definition,
  ]),
) as Record<string, ProposalBlockDefinition>;

/**
 * Extract registered block ids from an MDX proposal body. Unknown PascalCase MDX
 * tags are ignored here so callers can combine this helper with a separate
 * unknown-tag validation pass when needed.
 */
export function extractProposalBlockIds(body: string): ExtractedProposalBlockId[] {
  return parseMdxBody(body).flatMap((segment) => {
    if (segment.kind !== "block") return [];

    const definition = getProposalBlockDefinitionByTag(segment.tag);
    if (!definition) return [];

    return [
      {
        id: segment.id,
        tag: segment.tag,
        type: definition.type,
      },
    ];
  });
}
