// Thin re-export shim. The proposal block field-schema registry is now derived
// from a SINGLE source of `defineBlock()` specs in
// `@/components/proposals/blocks/registry`. This module preserves the original
// public surface (`PROPOSAL_BLOCK_REGISTRY`, the field-schema types, and the
// by-type / by-tag lookups) so every existing importer keeps working unchanged.
export {
  PROPOSAL_BLOCK_REGISTRY,
  getProposalBlockDefinition,
  getProposalBlockDefinitionByTag,
} from "@/components/proposals/blocks/registry";
export type {
  ProposalBlockDefinition,
  ProposalBlockFieldMap,
  ProposalBlockFieldSchema,
  ProposalBlockRegistry,
} from "@/components/proposals/blocks/registry";
