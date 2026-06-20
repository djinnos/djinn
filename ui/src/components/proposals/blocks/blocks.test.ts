import { describe, expect, it } from "vitest";

import { BLOCK_TYPES, getBlockByTag } from "@/lib/blockRegistry";

import {
  extractBlockTags,
  isPascalCaseTag,
  parseMdxBody,
} from "./parseMdxBody";

// All known P1 + P2 MDX tags
const KNOWN_TAGS = new Set(BLOCK_TYPES.map((b) => b.tag));

const SAMPLE_MDX = `# Proposal Title

Some introductory markdown text.

<RichText id="intro">
Welcome to the proposal. This is **rich text** content.
</RichText>

## Architecture

<Diagram id="arch-overview" type="mermaid">
graph TD
  A[Client] --> B[API]
  B --> C[Database]
</Diagram>

<AnnotatedCode id="handler" lang="rust">
fn handle_request(req: Request) -> Response {
  let data = parse(req);
  process(data)
}
</AnnotatedCode>

<DataModel id="user-schema">
Users table with id, email, name columns.
</DataModel>

<ApiEndpoint id="get-users" method="GET" path="/api/users">
Returns a list of all users.
</ApiEndpoint>

<Decisions id="auth-choice">
We chose JWT over session cookies for stateless auth.
</Decisions>

<FileTree id="project-layout">
\tsrc/
  main.rs
  lib.rs
tests/
</FileTree>

<QuestionForm id="open-questions">
Should we use Redis or Memcached for caching?
</QuestionForm>

Some trailing markdown.
`;

describe("block registry round-trip", () => {
  it("all tags in BLOCK_TYPES are unique", () => {
    const tags = BLOCK_TYPES.map((b) => b.tag);
    expect(new Set(tags).size).toBe(tags.length);
  });

  it("all P1 canonical block tags (RichText, Diagram, AnnotatedCode) are registered", () => {
    const p1Tags = ["RichText", "Diagram", "AnnotatedCode"];
    for (const tag of p1Tags) {
      expect(getBlockByTag(tag)).toBeDefined();
      expect(getBlockByTag(tag)!.requiredFields).toContain("id");
    }
  });

  it("parseMdxBody extracts all block tags from the sample MDX", () => {
    const segments = parseMdxBody(SAMPLE_MDX);
    const blockSegments = segments.filter((s) => s.kind === "block");

    const extractedTags = blockSegments.map((s) => s.tag);

    // All 8 block types should be present
    expect(extractedTags).toEqual([
      "RichText",
      "Diagram",
      "AnnotatedCode",
      "DataModel",
      "ApiEndpoint",
      "Decisions",
      "FileTree",
      "QuestionForm",
    ]);
  });

  it("all parsed block tags are known to the registry", () => {
    const segments = parseMdxBody(SAMPLE_MDX);
    const blockSegments = segments.filter((s) => s.kind === "block");

    for (const segment of blockSegments) {
      expect(
        KNOWN_TAGS.has(segment.tag),
        `Unknown block tag: ${segment.tag}`,
      ).toBe(true);
    }
  });

  it("all parsed blocks have structural equality with registry entries", () => {
    const segments = parseMdxBody(SAMPLE_MDX);
    const blockSegments = segments.filter((s) => s.kind === "block");

    for (const segment of blockSegments) {
      const def = getBlockByTag(segment.tag);
      expect(def).toBeDefined();

      // Structural equality: tag matches, id attribute is present
      expect(def!.tag).toBe(segment.tag);
      expect(segment.attributes.id).toBeDefined();
      expect(segment.id).toBe(segment.attributes.id);

      // Content is non-empty
      expect(segment.content.trim().length).toBeGreaterThan(0);
    }
  });

  it("markdown segments are preserved between blocks", () => {
    const segments = parseMdxBody(SAMPLE_MDX);
    const mdSegments = segments.filter((s) => s.kind === "markdown");

    // Should have at least 2 markdown segments: intro + trailing
    expect(mdSegments.length).toBeGreaterThanOrEqual(2);
    expect(mdSegments[0].text).toContain("# Proposal Title");
    expect(mdSegments[0].text).toContain("introductory markdown");
    // The last markdown segment should contain trailing text
    const last = mdSegments[mdSegments.length - 1];
    expect(last.text).toContain("trailing markdown");
  });

  it("block count in parsed body equals BLOCK_TYPES count", () => {
    const segments = parseMdxBody(SAMPLE_MDX);
    const blockSegments = segments.filter((s) => s.kind === "block");

    // The sample MDX contains exactly one of each block type
    expect(blockSegments.length).toBe(BLOCK_TYPES.length);
  });
});

describe("parseMdxBody — self-closing tags", () => {
  it("parses a self-closing <Diagram /> tag", () => {
    const body = 'Some text\n<Diagram id="d1" type="mermaid" />\nMore text';
    const segments = parseMdxBody(body);
    const blocks = segments.filter((s) => s.kind === "block");

    expect(blocks).toHaveLength(1);
    expect(blocks[0].tag).toBe("Diagram");
    expect(blocks[0].id).toBe("d1");
    expect(blocks[0].attributes.type).toBe("mermaid");
    // Self-closing form has no inner content
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
      '<AnnotatedCode id="a1" lang="rust">',
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
    // RichText has content, Diagram (self-closing) does not, AnnotatedCode does
    expect(blocks[0].content).toContain("Hello");
    expect(blocks[1].content).toBe("");
    expect(blocks[2].content).toContain("fn main");

    // Markdown segments: at least the title and the empty lines between blocks
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
    // Everything should be markdown
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
    expect(isPascalCaseTag("RichText")).toBe(true);
    expect(isPascalCaseTag("Diagram")).toBe(true);
    expect(isPascalCaseTag("AnnotatedCode")).toBe(true);
    expect(isPascalCaseTag("DataModel")).toBe(true);
    expect(isPascalCaseTag("ApiEndpoint")).toBe(true);
    expect(isPascalCaseTag("QuestionForm")).toBe(true);
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
  it("extracts all PascalCase block tags from a body", () => {
    const tags = extractBlockTags(SAMPLE_MDX);
    expect(tags).toEqual([
      "RichText",
      "Diagram",
      "AnnotatedCode",
      "DataModel",
      "ApiEndpoint",
      "Decisions",
      "FileTree",
      "QuestionForm",
    ]);
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

describe("P1 canonical tag integration with registry", () => {
  it("RichText, Diagram, and AnnotatedCode parse and resolve through the registry", () => {
    const body = [
      '<RichText id="rt-1">',
      "Some **rich** content.",
      "</RichText>",
      "",
      '<Diagram id="dg-1" type="mermaid">',
      "graph LR\n  A-->B",
      "</Diagram>",
      "",
      '<AnnotatedCode id="ac-1" lang="typescript">',
      "const x: number = 42;",
      "</AnnotatedCode>",
    ].join("\n");

    const segments = parseMdxBody(body);
    const blocks = segments.filter((s) => s.kind === "block");

    expect(blocks).toHaveLength(3);

    // Each P1 tag resolves to a registered block definition with a component
    for (const block of blocks) {
      const def = getBlockByTag(block.tag);
      expect(def, `${block.tag} should be in the registry`).toBeDefined();
      expect(def!.component, `${block.tag} should have a component`).toBeDefined();
      expect(def!.tag).toBe(block.tag);
    }

    // Verify specific P1 tags
    expect(blocks[0].tag).toBe("RichText");
    expect(blocks[0].id).toBe("rt-1");
    expect(blocks[0].content).toContain("rich");

    expect(blocks[1].tag).toBe("Diagram");
    expect(blocks[1].id).toBe("dg-1");
    expect(blocks[1].attributes.type).toBe("mermaid");

    expect(blocks[2].tag).toBe("AnnotatedCode");
    expect(blocks[2].id).toBe("ac-1");
    expect(blocks[2].attributes.lang).toBe("typescript");
  });
});
