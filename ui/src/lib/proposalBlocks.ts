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

export const PROPOSAL_BLOCK_REGISTRY = {
  "rich-text": {
    type: "rich-text",
    tag: "RichText",
    fields: {
      content: { type: "string" },
    },
  },
  diagram: {
    type: "diagram",
    tag: "Diagram",
    fields: {
      type: { type: "string", enum_values: ["mermaid", "plantuml", "svg"] },
      source: { type: "string" },
    },
  },
  "annotated-code": {
    type: "annotated-code",
    tag: "AnnotatedCode",
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
  },
  "data-model": {
    type: "data-model",
    tag: "DataModel",
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
  },
  "api-endpoint": {
    type: "api-endpoint",
    tag: "ApiEndpoint",
    fields: {
      method: { type: "string" },
      path: { type: "string" },
      description: { type: "string" },
      request_schema: { type: "string" },
      response_schema: { type: "string" },
    },
  },
  decisions: {
    type: "decisions",
    tag: "Decisions",
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
  },
  "file-tree": {
    type: "file-tree",
    tag: "FileTree",
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
  },
  "question-form": {
    type: "question-form",
    tag: "QuestionForm",
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
  },
} as const satisfies ProposalBlockRegistry;

export function getProposalBlockDefinition(type: string): ProposalBlockDefinition | undefined {
  return (PROPOSAL_BLOCK_REGISTRY as ProposalBlockRegistry)[type];
}

export function getProposalBlockDefinitionByTag(tag: string): ProposalBlockDefinition | undefined {
  return Object.values(PROPOSAL_BLOCK_REGISTRY).find((definition) => definition.tag === tag);
}
