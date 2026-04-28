import { describe, expect, it } from "vitest";

import {
  fileCitationToNodeId,
  parseCitationToken,
  segmentByCitations,
} from "./citationParser";

describe("parseCitationToken — file form", () => {
  it("parses a single line-range citation", () => {
    const c = parseCitationToken(
      "file",
      "server/src/foo.rs:42-58",
      "[[file:server/src/foo.rs:42-58]]",
    );
    expect(c).toEqual({
      kind: "file",
      raw: "[[file:server/src/foo.rs:42-58]]",
      path: "server/src/foo.rs",
      startLine: 42,
      endLine: 58,
    });
  });

  it("parses a bare-line citation as a degenerate range", () => {
    const c = parseCitationToken(
      "file",
      "src/foo.ts:7",
      "[[file:src/foo.ts:7]]",
    );
    expect(c).toMatchObject({
      kind: "file",
      path: "src/foo.ts",
      startLine: 7,
      endLine: 7,
    });
  });

  it("parses a path-only citation as no range", () => {
    const c = parseCitationToken(
      "file",
      "src/foo.ts",
      "[[file:src/foo.ts]]",
    );
    expect(c).toMatchObject({
      kind: "file",
      path: "src/foo.ts",
      startLine: null,
      endLine: null,
    });
  });

  it("rejects an empty body", () => {
    expect(parseCitationToken("file", "", "[[file:]]")).toBeNull();
  });

  it("rejects an inverted line range as path-only fallback", () => {
    // 50-10 is malformed → fall back to treating the full body as a path.
    const c = parseCitationToken(
      "file",
      "src/foo.ts:50-10",
      "[[file:src/foo.ts:50-10]]",
    );
    expect(c).toMatchObject({
      kind: "file",
      path: "src/foo.ts:50-10",
      startLine: null,
      endLine: null,
    });
  });
});

describe("parseCitationToken — symbol form", () => {
  it("parses a Type:Name pair", () => {
    const c = parseCitationToken(
      "symbol",
      "Function:check_permission",
      "[[symbol:Function:check_permission]]",
    );
    expect(c).toEqual({
      kind: "symbol",
      raw: "[[symbol:Function:check_permission]]",
      symbolKind: "Function",
      name: "check_permission",
    });
  });

  it("preserves Rust paths inside the name", () => {
    const c = parseCitationToken(
      "symbol",
      "Method:Auth::check",
      "[[symbol:Method:Auth::check]]",
    );
    expect(c).toMatchObject({
      kind: "symbol",
      symbolKind: "Method",
      name: "Auth::check",
    });
  });

  it("rejects a body missing the kind separator", () => {
    expect(parseCitationToken("symbol", "check", "[[symbol:check]]")).toBeNull();
  });

  it("rejects an empty kind", () => {
    expect(parseCitationToken("symbol", ":check", "[[symbol::check]]")).toBeNull();
  });

  it("rejects an empty name", () => {
    expect(parseCitationToken("symbol", "Function:", "[[symbol:Function:]]")).toBeNull();
  });
});

describe("segmentByCitations", () => {
  it("returns a single text segment for a citation-free message", () => {
    expect(segmentByCitations("Just plain prose.")).toEqual([
      { type: "text", text: "Just plain prose." },
    ]);
  });

  it("returns an empty array for empty input", () => {
    expect(segmentByCitations("")).toEqual([]);
  });

  it("splits leading/trailing prose around a single file citation", () => {
    const out = segmentByCitations(
      "See [[file:src/auth.rs:10-20]] for the check.",
    );
    expect(out).toHaveLength(3);
    expect(out[0]).toEqual({ type: "text", text: "See " });
    expect(out[1]).toMatchObject({ type: "citation" });
    if (out[1].type === "citation") {
      expect(out[1].citation).toMatchObject({
        kind: "file",
        path: "src/auth.rs",
        startLine: 10,
        endLine: 20,
      });
    }
    expect(out[2]).toEqual({ type: "text", text: " for the check." });
  });

  it("handles multiple citations of mixed forms in order", () => {
    const out = segmentByCitations(
      "[[file:a.rs:1-2]] then [[symbol:Function:foo]] last",
    );
    const cites = out.filter((s) => s.type === "citation");
    expect(cites).toHaveLength(2);
    expect((cites[0] as { citation: { kind: string } }).citation.kind).toBe("file");
    expect((cites[1] as { citation: { kind: string } }).citation.kind).toBe("symbol");
  });

  it("preserves malformed tokens as text", () => {
    const out = segmentByCitations("noise [[file:]] tail");
    // Empty file body → malformed, becomes a text segment that
    // coalesces with surrounding prose.
    expect(out).toEqual([{ type: "text", text: "noise [[file:]] tail" }]);
  });

  it("coalesces adjacent text segments", () => {
    const out = segmentByCitations("a [[bogus:thing]] b");
    expect(out).toEqual([{ type: "text", text: "a [[bogus:thing]] b" }]);
  });

  it("ignores wikilink-style brackets that aren't citations", () => {
    const out = segmentByCitations("see [[ADR-051]] for details");
    expect(out).toEqual([{ type: "text", text: "see [[ADR-051]] for details" }]);
  });
});

describe("fileCitationToNodeId", () => {
  it("prefixes the path with `file:`", () => {
    expect(
      fileCitationToNodeId({
        kind: "file",
        raw: "[[file:src/foo.rs:1-2]]",
        path: "src/foo.rs",
        startLine: 1,
        endLine: 2,
      }),
    ).toBe("file:src/foo.rs");
  });
});
