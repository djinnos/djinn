import type { BlockProps } from "@/components/proposals/blocks/types";
import {
  AnnotatedCode,
  ApiEndpointBlock,
  DataModelBlock,
  DecisionsBlock,
  Diagram,
  FileTreeBlock,
  QuestionFormBlock,
  RichText,
} from "@/components/proposals/blocks";

export interface BlockTypeDefinition {
  tag: string;
  displayName: string;
  requiredFields: string[];
  component: React.ComponentType<BlockProps>;
}

/**
 * Registry of all known block types (P1 + P2). Each entry maps an MDX tag name
 * to its display metadata, required fields, and the React component responsible
 * for rendering it.
 *
 * All blocks require an `id` attribute so that feedback comments and the debate
 * trail can anchor to a specific block across proposal revisions.
 */
export const BLOCK_TYPES: BlockTypeDefinition[] = [
  // P1 blocks
  {
    tag: "rich-text",
    displayName: "Rich Text",
    requiredFields: ["id"],
    component: RichText,
  },
  {
    tag: "diagram",
    displayName: "Diagram",
    requiredFields: ["id"],
    component: Diagram,
  },
  {
    tag: "annotated-code",
    displayName: "Annotated Code",
    requiredFields: ["id"],
    component: AnnotatedCode,
  },
  // P2 blocks
  {
    tag: "data-model",
    displayName: "Data Model",
    requiredFields: ["id"],
    component: DataModelBlock,
  },
  {
    tag: "api-endpoint",
    displayName: "API Endpoint",
    requiredFields: ["id"],
    component: ApiEndpointBlock,
  },
  {
    tag: "decisions",
    displayName: "Decisions",
    requiredFields: ["id"],
    component: DecisionsBlock,
  },
  {
    tag: "file-tree",
    displayName: "File Tree",
    requiredFields: ["id"],
    component: FileTreeBlock,
  },
  {
    tag: "question-form",
    displayName: "Open Questions",
    requiredFields: ["id"],
    component: QuestionFormBlock,
  },
];

/**
 * Look up a block type definition by its MDX tag name.
 *
 * Returns `undefined` when no registered block type matches the given tag.
 */
export function getBlockByTag(tag: string): BlockTypeDefinition | undefined {
  return BLOCK_TYPES.find((b) => b.tag === tag);
}
