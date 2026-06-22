import { describe, expect, it } from "vitest";
import { render } from "@/test/test-utils";

import { makeLineHighlighter } from "./diffHighlight";

describe("makeLineHighlighter", () => {
  it("tokenizes a line into coloured Prism token spans for a known language", () => {
    const highlight = makeLineHighlighter("rust");
    const { container } = render(<span>{highlight("let sum = a + b;")}</span>);
    // Prism tokens render as `.token` spans (RSH's `useInlineStyles` collapses
    // the grammar class to `token` and moves the oneDark colour to an inline
    // style — exactly as the standalone Code block does).
    const tokens = container.querySelectorAll("span.token");
    expect(tokens.length).toBeGreaterThan(0);
    // The `let` keyword is one of them, carrying an inline oneDark colour.
    const keyword = Array.from(tokens).find((t) => t.textContent === "let");
    expect(keyword).toBeTruthy();
    expect((keyword as HTMLElement).style.color).not.toBe("");
    // The full line text is preserved across the token spans.
    expect(container.textContent).toBe("let sum = a + b;");
  });

  it("caches the result so repeated calls return identical nodes", () => {
    const highlight = makeLineHighlighter("typescript");
    const first = highlight("const x = 1;");
    const second = highlight("const x = 1;");
    // Same memoized React node reference (no re-tokenization).
    expect(first).toBe(second);
  });

  it("returns the raw text for an empty/unknown/plaintext language", () => {
    for (const lang of [undefined, "", "text", "plaintext", "diff"]) {
      const highlight = makeLineHighlighter(lang);
      // Identity: the exact string back, no token spans.
      expect(highlight("let x = 1;")).toBe("let x = 1;");
    }
  });

  it("returns the raw text for a language refractor does not know", () => {
    const highlight = makeLineHighlighter("not-a-real-language");
    expect(highlight("let x = 1;")).toBe("let x = 1;");
  });
});
