import type { ComponentType } from "react";

import type { BlockProps } from "./types";
// Import each component directly from its own module (NOT the `./index` barrel)
// so this single-source registry has no import cycle through the barrel that
// also re-exports it.
import { AnnotatedCode } from "./AnnotatedCode";
import { ApiEndpointBlock } from "./ApiEndpointBlock";
import { CalloutBlock } from "./CalloutBlock";
import { ChecklistBlock } from "./ChecklistBlock";
import { ColumnsBlock } from "./ColumnsBlock";
import { DataModelBlock } from "./DataModelBlock";
import { DecisionsBlock } from "./DecisionsBlock";
import { Diagram } from "./Diagram";
import { DiffBlock } from "./DiffBlock";
import { FileTreeBlock } from "./FileTreeBlock";
import { HtmlBlock } from "./HtmlBlock";
import { JsonExplorerBlock } from "./JsonExplorerBlock";
import { OpenApiSpecBlock } from "./OpenApiSpecBlock";
import { QuestionFormBlock } from "./QuestionFormBlock";
import { RichText } from "./RichText";
import { TableBlock } from "./TableBlock";
import { TabsBlock } from "./TabsBlock";
import { WireframeBlock } from "./WireframeBlock";

// ---------------------------------------------------------------------------
// Field-schema contract
//
// One block is now defined ONCE, as a `defineBlock()` spec, and this module
// DERIVES every previously hand-maintained structure from that single source:
//   - `PROPOSAL_BLOCK_REGISTRY`   (field schemas; re-exported by `lib/proposalBlocks`)
//   - `BLOCK_COMPONENTS`          (tag → React component; re-exported by `lib/blockRegistry`)
//   - `BLOCK_DISPLAY_NAMES`       (tag → human label; re-exported by `lib/blockRegistry`)
//   - by-tag / by-type lookups
//
// The field-schema shape is IDENTICAL to the legacy `PROPOSAL_BLOCK_REGISTRY`
// data — this is a pure refactor mirroring the Rust proposal block registry.
// ---------------------------------------------------------------------------

export type ProposalBlockFieldSchema =
  | {
      type: "string";
      enum_values?: readonly string[];
    }
  | {
      type: "boolean";
    }
  | {
      type: "object";
      fields: ProposalBlockFieldMap;
    }
  | {
      type: "array";
      items: ProposalBlockFieldSchema;
    };

export type ProposalBlockFieldMap = Record<string, ProposalBlockFieldSchema>;

export interface ProposalBlockDefinition {
  type: string;
  tag: string;
  fields: ProposalBlockFieldMap;
}

export type ProposalBlockRegistry = Record<string, ProposalBlockDefinition>;

/**
 * The full single-source spec for one proposal block: the field-schema metadata
 * (`type`, `tag`, `fields`) plus the presentation concerns (`displayName`,
 * `component`) that used to live in separate maps in `lib/blockRegistry.ts`.
 */
export interface ProposalBlockSpec extends ProposalBlockDefinition {
  /** Human-readable label shown in the block palette / badges. */
  displayName: string;
  /** React component that renders this block. */
  component: ComponentType<BlockProps>;
}

/**
 * Identity helper that defines ONE block in ONE place. Returns the spec
 * unchanged; its sole purpose is to anchor inference and make each block a
 * single, self-contained declaration.
 */
export function defineBlock(spec: ProposalBlockSpec): ProposalBlockSpec {
  return spec;
}

// ---------------------------------------------------------------------------
// The single source of truth — one `defineBlock()` per block.
//
// ORDER MATTERS: `QuestionForm` MUST stay LAST (it is the open-questions form
// and is rendered after every other block). The derived structures preserve
// this insertion order.
// ---------------------------------------------------------------------------

export const PROPOSAL_BLOCK_SPECS: readonly ProposalBlockSpec[] = [
  defineBlock({
    type: "rich-text",
    tag: "RichText",
    displayName: "Rich Text",
    component: RichText,
    fields: {
      content: { type: "string" },
    },
  }),
  defineBlock({
    type: "diagram",
    tag: "Diagram",
    displayName: "Diagram",
    component: Diagram,
    fields: {
      type: { type: "string", enum_values: ["mermaid", "plantuml", "svg"] },
      source: { type: "string" },
    },
  }),
  defineBlock({
    type: "annotated-code",
    tag: "AnnotatedCode",
    displayName: "Code",
    component: AnnotatedCode,
    fields: {
      language: { type: "string" },
      code: { type: "string" },
      annotations: {
        type: "array",
        items: {
          type: "object",
          fields: {
            line: { type: "string" },
            note: { type: "string" },
          },
        },
      },
    },
  }),
  defineBlock({
    type: "data-model",
    tag: "DataModel",
    displayName: "Data Model",
    component: DataModelBlock,
    fields: {
      name: { type: "string" },
      fields: {
        type: "array",
        items: {
          type: "object",
          fields: {
            name: { type: "string" },
            type: { type: "string" },
            optional: { type: "boolean" },
            description: { type: "string" },
          },
        },
      },
    },
  }),
  defineBlock({
    type: "api-endpoint",
    tag: "ApiEndpoint",
    displayName: "API Endpoint",
    component: ApiEndpointBlock,
    fields: {
      method: { type: "string" },
      path: { type: "string" },
      description: { type: "string" },
      request_schema: { type: "string" },
      response_schema: { type: "string" },
    },
  }),
  defineBlock({
    type: "decisions",
    tag: "Decisions",
    displayName: "Decisions",
    component: DecisionsBlock,
    fields: {
      items: {
        type: "array",
        items: {
          type: "object",
          fields: {
            decision: { type: "string" },
            rationale: { type: "string" },
            status: { type: "string" },
          },
        },
      },
    },
  }),
  defineBlock({
    type: "file-tree",
    tag: "FileTree",
    displayName: "File Tree",
    component: FileTreeBlock,
    fields: {
      root: { type: "string" },
      entries: {
        type: "array",
        items: {
          type: "object",
          fields: {
            path: { type: "string" },
            kind: { type: "string", enum_values: ["file", "dir"] },
          },
        },
      },
    },
  }),
  defineBlock({
    type: "diff",
    tag: "Diff",
    displayName: "Diff",
    component: DiffBlock,
    fields: {
      filename: { type: "string" },
      lang: { type: "string" },
    },
  }),
  defineBlock({
    type: "callout",
    tag: "Callout",
    displayName: "Callout",
    component: CalloutBlock,
    fields: {
      tone: {
        type: "string",
        enum_values: ["info", "decision", "risk", "warning", "success"],
      },
    },
  }),
  defineBlock({
    type: "table",
    tag: "Table",
    displayName: "Table",
    component: TableBlock,
    fields: {},
  }),
  defineBlock({
    type: "checklist",
    tag: "Checklist",
    displayName: "Checklist",
    component: ChecklistBlock,
    fields: {},
  }),
  defineBlock({
    type: "json-explorer",
    tag: "JsonExplorer",
    displayName: "JSON Explorer",
    component: JsonExplorerBlock,
    fields: {},
  }),
  defineBlock({
    type: "html",
    tag: "Html",
    displayName: "HTML",
    component: HtmlBlock,
    fields: {},
  }),
  defineBlock({
    type: "openapi-spec",
    tag: "OpenApi",
    displayName: "OpenAPI Spec",
    component: OpenApiSpecBlock,
    fields: {},
  }),
  defineBlock({
    type: "tabs",
    tag: "Tabs",
    displayName: "Tabs",
    component: TabsBlock,
    fields: {
      tabs: { type: "string" },
    },
  }),
  defineBlock({
    type: "columns",
    tag: "Columns",
    displayName: "Columns",
    component: ColumnsBlock,
    fields: {
      columns: { type: "string" },
    },
  }),
  defineBlock({
    type: "wireframe",
    tag: "Wireframe",
    displayName: "Wireframe",
    component: WireframeBlock,
    fields: {
      surface: {
        type: "string",
        enum_values: ["browser", "desktop", "mobile", "popover", "panel"],
      },
    },
  }),
  // QuestionForm MUST remain LAST.
  defineBlock({
    type: "question-form",
    tag: "QuestionForm",
    displayName: "Open Questions",
    component: QuestionFormBlock,
    fields: {
      title: { type: "string" },
      questions: {
        type: "array",
        items: {
          type: "object",
          fields: {
            question: { type: "string" },
            kind: { type: "string", enum_values: ["text", "single", "multi"] },
            options: { type: "array", items: { type: "string" } },
          },
        },
      },
    },
  }),
];

// ---------------------------------------------------------------------------
// Derived structures — never hand-maintained; always built from the specs above.
// ---------------------------------------------------------------------------

/**
 * Field-schema registry keyed by stable block type. Mirrors the Rust proposal
 * block registry and replaces the legacy hand-maintained map. Re-exported as the
 * public `PROPOSAL_BLOCK_REGISTRY` from `@/lib/proposalBlocks`.
 */
export const PROPOSAL_BLOCK_REGISTRY: ProposalBlockRegistry = Object.fromEntries(
  PROPOSAL_BLOCK_SPECS.map((spec) => [
    spec.type,
    { type: spec.type, tag: spec.tag, fields: spec.fields },
  ]),
);

/** Tag → React component. Replaces the legacy `BLOCK_COMPONENTS` map. */
export const BLOCK_COMPONENTS: Record<string, ComponentType<BlockProps>> =
  Object.fromEntries(
    PROPOSAL_BLOCK_SPECS.map((spec) => [spec.tag, spec.component]),
  );

/** Tag → human label. Replaces the legacy `BLOCK_DISPLAY_NAMES` map. */
export const BLOCK_DISPLAY_NAMES: Record<string, string> = Object.fromEntries(
  PROPOSAL_BLOCK_SPECS.map((spec) => [spec.tag, spec.displayName]),
);

const SPECS_BY_TYPE = new Map(
  PROPOSAL_BLOCK_SPECS.map((spec) => [spec.type, spec]),
);
const SPECS_BY_TAG = new Map(
  PROPOSAL_BLOCK_SPECS.map((spec) => [spec.tag, spec]),
);

export function getProposalBlockDefinition(
  type: string,
): ProposalBlockDefinition | undefined {
  return PROPOSAL_BLOCK_REGISTRY[type];
}

export function getProposalBlockDefinitionByTag(
  tag: string,
): ProposalBlockDefinition | undefined {
  const spec = SPECS_BY_TAG.get(tag);
  return spec
    ? { type: spec.type, tag: spec.tag, fields: spec.fields }
    : undefined;
}

export function getProposalBlockSpec(type: string): ProposalBlockSpec | undefined {
  return SPECS_BY_TYPE.get(type);
}

export function getProposalBlockSpecByTag(
  tag: string,
): ProposalBlockSpec | undefined {
  return SPECS_BY_TAG.get(tag);
}
