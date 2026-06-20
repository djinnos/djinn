import { describe, expect, it } from "vitest";

import {
  PROPOSAL_BLOCK_REGISTRY,
  getProposalBlockDefinitionByTag,
} from "@/lib/proposalBlocks";

import {
  extractBlockTags,
  isPascalCaseTag,
  parseMdxBody,
} from "./parseMdxBody";

// ------------------------------------------------------------------------------
// Canonical v1 tag set — single source of truth for parity assertions.
// If a new block type is added, this array MUST be updated and the Rust
// registry must be updated in the same commit to prevent tag drift.
// ------------------------------------------------------------------------------
export const CANONICAL_V1_TAGS = [
  "RichText",
  "Diagram",
  "AnnotatedCode",
  "DataModel",
  "ApiEndpoint",
  "Decisions",
  "FileTree",
  "QuestionForm",
] as const;

export type CanonicalV1Tag = (typeof CANONICAL_V1_TAGS)[number];

// Import the canonical fixture as a raw string so the test suite exercises
// the exact same MDX sample that backend validation tests use.
const CANONICAL_MDX = (await import("./__fixtures__/canonicalProposal.mdx?raw"))
  .default as string;

// ------------------------------------------------------------------------------
// Parity: canonical tag set
// ------------------------------------------------------------------------------

describe("canonical tag set parity", () => {
  it("PROPOSAL_BLOCK_REGISTRY contains exactly the eight canonical v1 tags", () => {
    const registryTags = Object.values(PROPOSAL_BLOCK_REGISTRY).map(
      (def) => def.tag,
    );
    expect(registryTags.sort()).toEqual([...CANONICAL_V1_TAGS].sort());
  });

  it("every canonical tag resolves to a registry definition", () => {
    for (const tag of CANONICAL_V1_TAGS) {
      const def = getProposalBlockDefinitionByTag(tag);
      expect(def, `${tag} should be in PROPOSAL_BLOCK_REGISTRY`).toBeDefined();
      expect(def!.tag).toBe(tag);
    }
  });

  it("no extra tags exist in the registry beyond the canonical set", () => {
    const registryTags = Object.values(PROPOSAL_BLOCK_REGISTRY).map(
      (def) => def.tag,
    );
    expect(registryTags).toHaveLength(CANONICAL_V1_TAGS.length);
    for (const tag of registryTags) {
      expect(
        CANONICAL_V1_TAGS.includes(tag as CanonicalV1Tag),
        `${tag} is not in the canonical v1 tag set`,
      ).toBe(true);
    }
  });
});

// ------------------------------------------------------------------------------
// Round-trip: canonical MDX → parser → registry
// ------------------------------------------------------------------------------

describe("canonical proposal.mdx round-trip", () => {
  it("parses all eight v1 block types from the canonical fixture", () => {
    const segments = parseMdxBody(CANONICAL_MDX);
    const blockSegments = segments.filter((s) => s.kind === "block");
    const extractedTags = blockSegments.map((s) => s.tag);

    expect(extractedTags).toEqual(CANONICAL_V1_TAGS);
  });

  it("every parsed block resolves to the TS registry definition", () => {
    const segments = parseMdxBody(CANONICAL_MDX);
    const blockSegments = segments.filter((s) => s.kind === "block");

    for (const segment of blockSegments) {
      const def = getProposalBlockDefinitionByTag(segment.tag);
      expect(
        def,
        `${segment.tag} should resolve to a registry definition`,
      ).toBeDefined();
      expect(def!.tag).toBe(segment.tag);
      expect(def!.type).toBeDefined();
    }
  });

  it("asserts parsed tag, block id, and registry mapping consistency for all eight v1 types", () => {
    const segments = parseMdxBody(CANONICAL_MDX);
    const blockSegments = segments.filter((s) => s.kind === "block");

    expect(blockSegments).toHaveLength(CANONICAL_V1_TAGS.length);

    // Build a map for easy lookup by tag
    const byTag = new Map(
      blockSegments.map((s) => [s.tag, s as (typeof blockSegments)[number]]),
    );

    // RichText
    const richText = byTag.get("RichText");
    expect(richText).toBeDefined();
    expect(richText!.id).toBe("intro");
    expect(richText!.content).toContain("rich text");
    expect(getProposalBlockDefinitionByTag("RichText")!.type).toBe("rich-text");

    // Diagram
    const diagram = byTag.get("Diagram");
    expect(diagram).toBeDefined();
    expect(diagram!.id).toBe("arch-overview");
    expect(diagram!.attributes.type).toBe("mermaid");
    expect(diagram!.content).toContain("graph TD");
    expect(getProposalBlockDefinitionByTag("Diagram")!.type).toBe("diagram");

    // AnnotatedCode
    const annotatedCode = byTag.get("AnnotatedCode");
    expect(annotatedCode).toBeDefined();
    expect(annotatedCode!.id).toBe("handler");
    expect(annotatedCode!.attributes.language).toBe("rust");
    expect(annotatedCode!.content).toContain("fn handle_request");
    expect(getProposalBlockDefinitionByTag("AnnotatedCode")!.type).toBe(
      "annotated-code",
    );

    // DataModel
    const dataModel = byTag.get("DataModel");
    expect(dataModel).toBeDefined();
    expect(dataModel!.id).toBe("user-schema");
    expect(dataModel!.attributes.name).toBe("User");
    expect(dataModel!.content).toContain("uuid");
    expect(getProposalBlockDefinitionByTag("DataModel")!.type).toBe(
      "data-model",
    );

    // ApiEndpoint
    const apiEndpoint = byTag.get("ApiEndpoint");
    expect(apiEndpoint).toBeDefined();
    expect(apiEndpoint!.id).toBe("get-users");
    expect(apiEndpoint!.attributes.method).toBe("GET");
    expect(apiEndpoint!.attributes.path).toBe("/api/users");
    expect(apiEndpoint!.content).toContain("Returns a list");
    expect(getProposalBlockDefinitionByTag("ApiEndpoint")!.type).toBe(
      "api-endpoint",
    );

    // Decisions
    const decisions = byTag.get("Decisions");
    expect(decisions).toBeDefined();
    expect(decisions!.id).toBe("auth-choice");
    expect(decisions!.content).toContain("JWT");
    expect(getProposalBlockDefinitionByTag("Decisions")!.type).toBe(
      "decisions",
    );

    // FileTree
    const fileTree = byTag.get("FileTree");
    expect(fileTree).toBeDefined();
    expect(fileTree!.id).toBe("project-layout");
    expect(fileTree!.attributes.root).toBe("src");
    expect(fileTree!.content).toContain("main.rs");
    expect(getProposalBlockDefinitionByTag("FileTree")!.type).toBe("file-tree");

    // QuestionForm
    const questionForm = byTag.get("QuestionForm");
    expect(questionForm).toBeDefined();
    expect(questionForm!.id).toBe("open-questions");
    expect(questionForm!.attributes.title).toBe("Open Questions");
    expect(questionForm!.content).toContain("Redis");
    expect(getProposalBlockDefinitionByTag("QuestionForm")!.type).toBe(
      "question-form",
    );
  });

  it("markdown segments are preserved between blocks in the canonical fixture", () => {
    const segments = parseMdxBody(CANONICAL_MDX);
    const mdSegments = segments.filter((s) => s.kind === "markdown");

    expect(mdSegments.length).toBeGreaterThanOrEqual(2);
    expect(mdSegments[0].text).toContain("# Canonical Proposal");
    const last = mdSegments[mdSegments.length - 1];
    expect(last.text).toContain("trailing markdown");
  });
});

// ------------------------------------------------------------------------------
// Parser behaviour (not dependent on the canonical fixture)
// ------------------------------------------------------------------------------

describe("parseMdxBody — self-closing tags", () => {
  it("parses a self-closing <Diagram /> tag", () => {
    const body = 'Some text\n<Diagram id="d1" type="mermaid" />\nMore text';
    const segments = parseMdxBody(body);
    const blocks = segments.filter((s) => s.kind === "block");

    expect(blocks).toHaveLength(1);
    expect(blocks[0].tag).toBe("Diagram");
    expect(blocks[0].id).toBe("d1");
    expect(blocks[0].attributes.type).toBe("mermaid");
    expect(blocks[0].content).toBe("");
  });

  it("parses a self-closing <RichText /> tag with no attributes", () => {
    const body = "<RichText id=\"empty\" />";
    const segments = parseMdxBody(body);
    const blocks = segments.filter((s) => s.kind === "block");

    expect(blocks).toHaveLength(1);
    expect(blocks[0].tag).toBe("RichText");
    expect(blocks[0].content).toBe("");
  });

  it("parses a mixed body with self-closing and normal-form P1 tags", () => {
    const body = [
      "# Title",
      "",
      '<RichText id="r1">',
      "Hello **world**",
      "</RichText>",
      "",
      '<Diagram id="d1" type="mermaid" />',
      "",
      '<AnnotatedCode id="a1" language="rust">',
      "fn main() {}",
      "</AnnotatedCode>",
    ].join("\n");

    const segments = parseMdxBody(body);
    const blocks = segments.filter((s) => s.kind === "block");
    const mds = segments.filter((s) => s.kind === "markdown");

    expect(blocks.map((b) => b.tag)).toEqual([
      "RichText",
      "Diagram",
      "AnnotatedCode",
    ]);
    expect(blocks[0].content).toContain("Hello");
    expect(blocks[1].content).toBe("");
    expect(blocks[2].content).toContain("fn main");

    expect(mds.length).toBeGreaterThanOrEqual(1);
    expect(mds[0].text).toContain("# Title");
  });
});

describe("parseMdxBody — lowercase and non-PascalCase tags are NOT parsed as blocks", () => {
  it("does not extract lowercase HTML tags as blocks", () => {
    const body =
      '<div class="container">Hello</div>\n<span>world</span>\n<p>paragraph</p>';
    const segments = parseMdxBody(body);
    const blocks = segments.filter((s) => s.kind === "block");

    expect(blocks).toHaveLength(0);
    expect(segments).toHaveLength(1);
    expect(segments[0].kind).toBe("markdown");
  });

  it("does not extract kebab-case tags as blocks", () => {
    const body = "<my-component>content</my-component>";
    const segments = parseMdxBody(body);
    const blocks = segments.filter((s) => s.kind === "block");

    expect(blocks).toHaveLength(0);
  });

  it("preserves angle brackets in markdown content", () => {
    const body =
      "Use `Vec<T>` in Rust.\nAlso see `Option<None>` for generics.";
    const segments = parseMdxBody(body);

    expect(segments).toHaveLength(1);
    expect(segments[0].kind).toBe("markdown");
    expect(segments[0].text).toContain("Vec<T>");
  });
});

describe("isPascalCaseTag", () => {
  it("returns true for canonical block tags", () => {
    for (const tag of CANONICAL_V1_TAGS) {
      expect(isPascalCaseTag(tag)).toBe(true);
    }
  });

  it("returns true for single PascalCase word", () => {
    expect(isPascalCaseTag("Diagram")).toBe(true);
  });

  it("returns false for lowercase tags", () => {
    expect(isPascalCaseTag("div")).toBe(false);
    expect(isPascalCaseTag("span")).toBe(false);
    expect(isPascalCaseTag("richText")).toBe(false);
  });

  it("returns false for kebab-case tags", () => {
    expect(isPascalCaseTag("rich-text")).toBe(false);
    expect(isPascalCaseTag("my-component")).toBe(false);
  });

  it("returns false for empty string", () => {
    expect(isPascalCaseTag("")).toBe(false);
  });
});

describe("extractBlockTags", () => {
  it("extracts all PascalCase block tags from the canonical fixture", () => {
    const tags = extractBlockTags(CANONICAL_MDX);
    expect(tags).toEqual(CANONICAL_V1_TAGS);
  });

  it("extracts self-closing tags", () => {
    const body = '<Diagram id="d1" />\n<RichText id="r1">content</RichText>';
    const tags = extractBlockTags(body);
    expect(tags).toEqual(["Diagram", "RichText"]);
  });

  it("returns empty array for pure markdown body", () => {
    const body = "# Hello\n\nJust some markdown.\n\n```code```";
    const tags = extractBlockTags(body);
    expect(tags).toEqual([]);
  });

  it("returns empty array for empty string", () => {
    expect(extractBlockTags("")).toEqual([]);
  });
});
