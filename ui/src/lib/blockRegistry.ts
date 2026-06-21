import type { ComponentType } from "react";

import type { BlockProps } from "@/components/proposals/blocks/types";
import {
  AnnotatedCode,
  ApiEndpointBlock,
  CalloutBlock,
  ChecklistBlock,
  CodeSnippetBlock,
  DataModelBlock,
  DecisionsBlock,
  Diagram,
  DiffBlock,
  FileTreeBlock,
  QuestionFormBlock,
  RichText,
  TableBlock,
} from "@/components/proposals/blocks";
import {
  PROPOSAL_BLOCK_REGISTRY,
  type ProposalBlockDefinition,
} from "@/components/proposals/blocks/blockRegistry";

type ProposalBlockType = keyof typeof PROPOSAL_BLOCK_REGISTRY;

export interface BlockTypeDefinition extends ProposalBlockDefinition {
  displayName: string;
  requiredFields: string[];
  component: ComponentType<BlockProps>;
}

const BLOCK_COMPONENTS: Record<ProposalBlockType, ComponentType<BlockProps>> = {
  "rich-text": RichText,
  diagram: Diagram,
  "annotated-code": AnnotatedCode,
  "data-model": DataModelBlock,
  "api-endpoint": ApiEndpointBlock,
  decisions: DecisionsBlock,
  "file-tree": FileTreeBlock,
  diff: DiffBlock,
  callout: CalloutBlock,
  table: TableBlock,
  checklist: ChecklistBlock,
  code: CodeSnippetBlock,
  "question-form": QuestionFormBlock,
};

const BLOCK_DISPLAY_NAMES: Record<ProposalBlockType, string> = {
  "rich-text": "Rich Text",
  diagram: "Diagram",
  "annotated-code": "Annotated Code",
  "data-model": "Data Model",
  "api-endpoint": "API Endpoint",
  decisions: "Decisions",
  "file-tree": "File Tree",
  diff: "Diff",
  callout: "Callout",
  table: "Table",
  checklist: "Checklist",
  code: "Code",
  "question-form": "Open Questions",
};

/**
 * Render registry derived from the canonical proposal block contract.
 *
 * The MDX tag names come exclusively from PROPOSAL_BLOCK_REGISTRY, which mirrors
 * the Rust proposal_blocks registry. Component/display metadata is keyed by the
 * stable block type so this file cannot drift into a second, contradictory tag
 * list.
 *
 * All blocks require an `id` attribute so that feedback comments and the debate
 * trail can anchor to a specific block across proposal revisions.
 */
const TYPED_PROPOSAL_BLOCK_REGISTRY = PROPOSAL_BLOCK_REGISTRY as unknown as Record<
  ProposalBlockType,
  ProposalBlockDefinition
>;

export const BLOCK_TYPES: BlockTypeDefinition[] = (
  Object.keys(TYPED_PROPOSAL_BLOCK_REGISTRY) as ProposalBlockType[]
).map((type) => {
  const definition = TYPED_PROPOSAL_BLOCK_REGISTRY[type];
  return {
    ...definition,
    displayName: BLOCK_DISPLAY_NAMES[type],
    requiredFields: ["id"],
    component: BLOCK_COMPONENTS[type],
  };
});

const BLOCK_TYPES_BY_TAG = new Map(BLOCK_TYPES.map((block) => [block.tag, block]));

/**
 * Look up a block type definition by its canonical PascalCase MDX tag name.
 *
 * Returns `undefined` when no registered block type matches the given tag.
 */
export function getBlockByTag(tag: string): BlockTypeDefinition | undefined {
  return BLOCK_TYPES_BY_TAG.get(tag);
}
