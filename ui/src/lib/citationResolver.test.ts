import { describe, expect, it, vi } from "vitest";

import {
  HIGH_CONFIDENCE_CUTOFF,
  resolveCitation,
  type CodeGraphSearchClient,
} from "./citationResolver";
import type {
  FileCitation,
  SymbolCitation,
} from "./citationParser";
import type { SearchHit } from "@/components/pulse/pulseTypes";

function fileCitation(path: string, range?: [number, number]): FileCitation {
  return {
    kind: "file",
    raw: `[[file:${path}${range ? `:${range[0]}-${range[1]}` : ""}]]`,
    path,
    startLine: range?.[0] ?? null,
    endLine: range?.[1] ?? null,
  };
}

function symbolCitation(symbolKind: string, name: string): SymbolCitation {
  return {
    kind: "symbol",
    raw: `[[symbol:${symbolKind}:${name}]]`,
    symbolKind,
    name,
  };
}

function hit(key: string, score: number): SearchHit {
  return {
    key,
    kind: "function",
    display_name: key,
    score,
    file: null,
    match_kind: null,
  };
}

describe("resolveCitation — file form", () => {
  it("resolves directly without calling the search client", async () => {
    const client: CodeGraphSearchClient = {
      search: vi.fn().mockResolvedValue([]),
    };
    const out = await resolveCitation(
      fileCitation("src/auth.rs", [10, 20]),
      "proj",
      client,
    );
    expect(client.search).not.toHaveBeenCalled();
    expect(out).toEqual({
      status: "direct",
      citation: expect.objectContaining({ kind: "file", path: "src/auth.rs" }),
      nodeId: "file:src/auth.rs",
      startLine: 10,
      endLine: 20,
    });
  });
});

describe("resolveCitation — symbol form", () => {
  it("returns 'direct' for a single high-confidence hit", async () => {
    const client: CodeGraphSearchClient = {
      search: vi.fn().mockResolvedValue([hit("symbol:check", 0.92)]),
    };
    const out = await resolveCitation(
      symbolCitation("Function", "check"),
      "proj",
      client,
    );
    expect(out).toEqual({
      status: "direct",
      citation: expect.objectContaining({ kind: "symbol", name: "check" }),
      nodeId: "symbol:check",
    });
  });

  it("returns 'ambiguous' when more than one hit is returned", async () => {
    const client: CodeGraphSearchClient = {
      search: vi
        .fn()
        .mockResolvedValue([hit("symbol:a", 0.95), hit("symbol:b", 0.6)]),
    };
    const out = await resolveCitation(
      symbolCitation("Function", "check"),
      "proj",
      client,
    );
    expect(out.status).toBe("ambiguous");
    if (out.status === "ambiguous") {
      // Sorted high-to-low.
      expect(out.hits.map((h) => h.key)).toEqual(["symbol:a", "symbol:b"]);
    }
  });

  it("returns 'ambiguous' when the single hit's score is below the cutoff", async () => {
    const client: CodeGraphSearchClient = {
      search: vi
        .fn()
        .mockResolvedValue([hit("symbol:weak", HIGH_CONFIDENCE_CUTOFF - 0.01)]),
    };
    const out = await resolveCitation(
      symbolCitation("Function", "weak"),
      "proj",
      client,
    );
    expect(out.status).toBe("ambiguous");
  });

  it("returns 'missing' when no hits come back", async () => {
    const client: CodeGraphSearchClient = {
      search: vi.fn().mockResolvedValue([]),
    };
    const out = await resolveCitation(
      symbolCitation("Function", "ghost"),
      "proj",
      client,
    );
    expect(out.status).toBe("missing");
  });

  it("forwards the kind hint and limit to the search client", async () => {
    const client: CodeGraphSearchClient = {
      search: vi.fn().mockResolvedValue([hit("symbol:foo", 0.99)]),
    };
    await resolveCitation(symbolCitation("Class", "Foo"), "proj", client);
    expect(client.search).toHaveBeenCalledWith(
      "proj",
      expect.objectContaining({ symbolKind: "Class", name: "Foo" }),
    );
  });
});
