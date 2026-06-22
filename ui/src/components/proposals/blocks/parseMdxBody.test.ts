import { describe, expect, it } from "vitest";

import { parseMdxBody } from "./parseMdxBody";

const CANONICAL_MDX = (await import("./__fixtures__/canonicalProposal.mdx?raw"))
  .default as string;

// These cases target the exact failure modes of the previous regex parser
// (`/<([A-Z][A-Za-z0-9]*)([^>]*?)(?:\/>|>([\s\S]*?)<\/\1>)/g`). The MDX-AST
// parser must handle them while keeping `content` byte-identical to a slice of
// the source between each block's tags.

describe("parseMdxBody — AST parser fixes for regex limitations", () => {
  it("does not truncate an attribute value that contains '>'", () => {
    // The old `[^>]*?` attribute group stopped at the first '>', truncating
    // both the open tag and the captured content.
    const body = '<ApiEndpoint id="x" path="/a?to=>b">body</ApiEndpoint>';
    const segments = parseMdxBody(body);
    const blocks = segments.filter((s) => s.kind === "block");

    expect(blocks).toHaveLength(1);
    expect(blocks[0].tag).toBe("ApiEndpoint");
    expect(blocks[0].attributes.path).toBe("/a?to=>b");
    expect(blocks[0].content).toBe("body");
  });

  it("does not truncate at a nested same-named close tag in children", () => {
    // The old non-greedy `([\s\S]*?)<\/\1>` matched the FIRST `</Callout>`,
    // truncating the outer block. The children here contain a Callout-like
    // inner tag in *text* (a fenced code block) so the outer Callout's content
    // must include everything up to the real outer close tag.
    const body = [
      '<Callout id="outer" tone="info">',
      "Intro line.",
      "",
      "```mdx",
      '<Callout id="inner">nested</Callout>',
      "```",
      "",
      "Outro line.",
      "</Callout>",
    ].join("\n");

    const segments = parseMdxBody(body);
    const blocks = segments.filter((s) => s.kind === "block");

    expect(blocks).toHaveLength(1);
    expect(blocks[0].id).toBe("outer");
    expect(blocks[0].content).toContain("Intro line.");
    expect(blocks[0].content).toContain("<Callout id=\"inner\">nested</Callout>");
    expect(blocks[0].content).toContain("Outro line.");
  });

  it("preserves a {...} expression attribute as its source text", () => {
    // The old quoted-only ATTR_REGEX could not read JSX-expression attributes;
    // they were silently dropped. Forward-compat blocks (e.g. Tabs
    // `tabs={[...]}`) need the raw expression text retained.
    const body = '<Diagram id="d" config={{ "a": 1 }} />';
    const segments = parseMdxBody(body);
    const blocks = segments.filter((s) => s.kind === "block");

    expect(blocks).toHaveLength(1);
    expect(blocks[0].tag).toBe("Diagram");
    expect(blocks[0].attributes.id).toBe("d");
    expect(blocks[0].attributes.config).toBe('{ "a": 1 }');
    expect(blocks[0].content).toBe("");
    // The retained expression text is valid JSON and round-trips.
    expect(JSON.parse(blocks[0].attributes.config)).toEqual({ a: 1 });
  });

  it("slices children byte-for-byte from the source for every canonical block", () => {
    // Guards against markdown re-stringification: each block's `content` must
    // equal the exact substring of the source between its opening and closing
    // tags.
    const segments = parseMdxBody(CANONICAL_MDX);
    const blocks = segments.filter((s) => s.kind === "block");

    expect(blocks.length).toBeGreaterThan(0);

    for (const block of blocks) {
      if (block.content === "") continue; // self-closing
      const idx = CANONICAL_MDX.indexOf(block.content);
      expect(
        idx,
        `${block.tag} content should be an exact source substring`,
      ).toBeGreaterThanOrEqual(0);
      // The character immediately before the content is the '>' of the open
      // tag and the character at the content's end begins the closing tag.
      expect(CANONICAL_MDX[idx - 1]).toBe(">");
      expect(
        CANONICAL_MDX.slice(idx + block.content.length, idx + block.content.length + 2),
      ).toBe("</");
    }
  });
});
