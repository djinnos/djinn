import { describe, expect, it } from "vitest";

import { BLOCK_TYPES, getBlockByTag } from "@/lib/blockRegistry";
import { parseMdxBody } from "./parseMdxBody";

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
src/
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
