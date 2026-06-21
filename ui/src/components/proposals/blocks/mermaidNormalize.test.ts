import { describe, expect, it } from "vitest";

import { normalizeMermaidSource } from "./mermaidNormalize";

describe("normalizeMermaidSource — arrow/dash glyphs", () => {
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
    expect(normalizeMermaidSource("A{a → b}")).toBe('A{"a → b"}');
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

describe("normalizeMermaidSource — node-label auto-quoting", () => {
  it("quotes a rectangle label with parens and -D flag", () => {
    expect(normalizeMermaidSource("A[clippy -D warnings (exists)]")).toBe(
      'A["clippy -D warnings (exists)"]',
    );
  });

  it("quotes a label with a colon", () => {
    expect(normalizeMermaidSource("C[cargo-deny: advisories]")).toBe(
      'C["cargo-deny: advisories"]',
    );
  });

  it("quotes a round-shape (...) label", () => {
    expect(normalizeMermaidSource("A(do a thing: now)")).toBe(
      'A("do a thing: now")',
    );
  });

  it("quotes a rhombus {...} label", () => {
    expect(normalizeMermaidSource("A{ok? (maybe)}")).toBe('A{"ok? (maybe)"}');
  });

  it("quotes a stadium ([...]) label", () => {
    expect(normalizeMermaidSource("A([start: go])")).toBe('A(["start: go"])');
  });

  it("quotes a subroutine [[...]] label", () => {
    expect(normalizeMermaidSource("A[[call: fn()]]")).toBe('A[["call: fn()"]]');
  });

  it("quotes a circle ((...)) label", () => {
    expect(normalizeMermaidSource("A((node: x))")).toBe('A(("node: x"))');
  });

  it("quotes a hexagon {{...}} label", () => {
    expect(normalizeMermaidSource("A{{decide: y/n}}")).toBe(
      'A{{"decide: y/n"}}',
    );
  });

  it("quotes a cylinder [(...)] label", () => {
    expect(normalizeMermaidSource("A[(db: rows)]")).toBe('A[("db: rows")]');
  });

  it("leaves an already-quoted label untouched (idempotent)", () => {
    const src = 'A["already (quoted)"]';
    expect(normalizeMermaidSource(src)).toBe(src);
    // Running twice yields the same result.
    expect(normalizeMermaidSource(normalizeMermaidSource(src))).toBe(src);
  });

  it("re-running on already-quoted output is a no-op", () => {
    const once = normalizeMermaidSource("A[clippy -D warnings (exists)]");
    expect(normalizeMermaidSource(once)).toBe(once);
  });

  it("escapes inner double quotes to #quot;", () => {
    expect(normalizeMermaidSource('A[say "hi" loud]')).toBe(
      'A["say #quot;hi#quot; loud"]',
    );
  });

  it("does not quote an empty label", () => {
    expect(normalizeMermaidSource("A[]")).toBe("A[]");
  });

  it("quotes a simple alphanumeric label too (harmless, valid)", () => {
    // Even simple labels get quoted; Mermaid accepts quoted simple labels.
    expect(normalizeMermaidSource("A[plain]")).toBe('A["plain"]');
  });

  it("does not touch a pipe edge label |...|", () => {
    // The edge label between two nodes must NOT be quoted (would break syntax).
    expect(normalizeMermaidSource("A -->|yes: go| B")).toBe("A -->|yes: go| B");
  });

  it("quotes node labels on both ends of an edge", () => {
    expect(normalizeMermaidSource("A[do (x)] --> B[do (y)]")).toBe(
      'A["do (x)"] --> B["do (y)"]',
    );
  });
});

describe("normalizeMermaidSource — real LLM proposal repro", () => {
  // The exact source from a real proposal that previously failed to parse:
  // the `⟶` arrows AND the unquoted labels with `(`, `)`, `:`, `-D`, `--check`.
  const REPRO = [
    "graph TD",
    "  PR[PR push] ⟶ A[clippy -D warnings (exists)]",
    "  PR ⟶ B[hakari verify (exists)]",
    "  PR ⟶ C[cargo-deny: advisories + licenses (NEW)]",
    "  PR ⟶ D[cargo-deny bans: sqlx wrapper-allowlist ratchet (NEW)]",
    "  PR ⟶ E[check-boundaries OR grep guard: no raw SQL outside djinn-db (NEW)]",
    "  PR ⟶ F[sqlx prepare --check on every PR, not just merge_group (MOVED)]",
  ].join("\n");

  const EXPECTED = [
    "graph TD",
    '  PR["PR push"] --> A["clippy -D warnings (exists)"]',
    '  PR --> B["hakari verify (exists)"]',
    '  PR --> C["cargo-deny: advisories + licenses (NEW)"]',
    '  PR --> D["cargo-deny bans: sqlx wrapper-allowlist ratchet (NEW)"]',
    '  PR --> E["check-boundaries OR grep guard: no raw SQL outside djinn-db (NEW)"]',
    '  PR --> F["sqlx prepare --check on every PR, not just merge_group (MOVED)"]',
  ].join("\n");

  it("normalizes arrows AND quotes every node label", () => {
    expect(normalizeMermaidSource(REPRO)).toBe(EXPECTED);
  });

  it("each label line becomes quoted", () => {
    const out = normalizeMermaidSource(REPRO);
    expect(out).toContain('A["clippy -D warnings (exists)"]');
    expect(out).toContain('C["cargo-deny: advisories + licenses (NEW)"]');
    expect(out).toContain(
      'F["sqlx prepare --check on every PR, not just merge_group (MOVED)"]',
    );
    // No raw `⟶` glyphs survive.
    expect(out).not.toMatch(/⟶/);
  });

  it("is idempotent on the repro", () => {
    const once = normalizeMermaidSource(REPRO);
    expect(normalizeMermaidSource(once)).toBe(once);
  });
});
