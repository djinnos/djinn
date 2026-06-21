import { describe, expect, it } from "vitest";

import { normalizeMermaidSource } from "./mermaidNormalize";

describe("normalizeMermaidSource", () => {
  it("leaves ASCII edges untouched", () => {
    const src = "flowchart TD\n  A --> B\n  B -- label --> C";
    expect(normalizeMermaidSource(src)).toBe(src);
  });

  it("returns the source unchanged when no candidate glyph is present", () => {
    const src = "graph LR; x---y";
    expect(normalizeMermaidSource(src)).toBe(src);
  });

  it("normalizes the U+2192 right arrow to -->", () => {
    expect(normalizeMermaidSource("A → B")).toBe("A --> B");
  });

  it("normalizes the U+27F6 long arrow to -->", () => {
    expect(normalizeMermaidSource("A ⟶ B")).toBe("A --> B");
  });

  it("normalizes ⇒ and ➜ arrow variants to -->", () => {
    expect(normalizeMermaidSource("A ⇒ B")).toBe("A --> B");
    expect(normalizeMermaidSource("A ➜ B")).toBe("A --> B");
  });

  it("normalizes a dash+arrow run (—→) to -->", () => {
    expect(normalizeMermaidSource("A —→ B")).toBe("A --> B");
  });

  it("normalizes a bare em/en dash edge to --", () => {
    expect(normalizeMermaidSource("A — B")).toBe("A -- B");
    expect(normalizeMermaidSource("A – B")).toBe("A -- B");
  });

  it("preserves arrow glyphs inside bracket labels", () => {
    const src = 'A["price → total"] --> B';
    expect(normalizeMermaidSource(src)).toBe(src);
  });

  it("preserves arrow glyphs inside quoted and paren/brace labels", () => {
    expect(normalizeMermaidSource('A("a → b")')).toBe('A("a → b")');
    expect(normalizeMermaidSource("A{a → b}")).toBe("A{a → b}");
  });

  it("normalizes a structural arrow while preserving an in-label arrow", () => {
    const src = 'A["x → y"] → B';
    expect(normalizeMermaidSource(src)).toBe('A["x → y"] --> B');
  });

  it("does not rewrite a dash glyph embedded in a word", () => {
    // Em-dash inside a token (no surrounding whitespace) is left alone.
    expect(normalizeMermaidSource("flowchart TD")).toBe("flowchart TD");
    expect(normalizeMermaidSource("foo—bar")).toBe("foo—bar");
  });

  it("handles multiple edges across lines", () => {
    const src = "flowchart TD\n  A → B\n  B ⟶ C";
    expect(normalizeMermaidSource(src)).toBe(
      "flowchart TD\n  A --> B\n  B --> C",
    );
  });

  it("returns empty/undefined-safe for empty input", () => {
    expect(normalizeMermaidSource("")).toBe("");
  });
});
